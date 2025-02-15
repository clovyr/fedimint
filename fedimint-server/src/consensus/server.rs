use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use aleph_bft::Keychain as KeychainTrait;
use anyhow::{anyhow, bail};
use async_channel::{Receiver, Sender};
use bitcoin_hashes::sha256;
use fedimint_core::api::{FederationApiExt, GlobalFederationApi, WsFederationApi};
use fedimint_core::block::{AcceptedItem, Block, SchnorrSignature, SignedBlock};
use fedimint_core::config::ServerModuleInitRegistry;
use fedimint_core::db::{
    apply_migrations, Database, DatabaseTransaction, IDatabaseTransactionOpsCoreTyped,
};
use fedimint_core::encoding::Decodable;
use fedimint_core::endpoint_constants::AWAIT_SIGNED_BLOCK_ENDPOINT;
use fedimint_core::epoch::{ConsensusItem, SerdeSignature, SerdeSignatureShare};
use fedimint_core::fmt_utils::OptStacktrace;
use fedimint_core::module::audit::Audit;
use fedimint_core::module::registry::{
    ModuleDecoderRegistry, ModuleRegistry, ServerModuleRegistry,
};
use fedimint_core::module::{ApiRequestErased, SerdeModuleEncoding};
use fedimint_core::query::FilterMap;
use fedimint_core::task::{sleep, spawn, RwLock, TaskGroup, TaskHandle};
use fedimint_core::util::SafeUrl;
use fedimint_core::{timing, PeerId};
use futures::StreamExt;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::atomic_broadcast::data_provider::{DataProvider, UnitData};
use crate::atomic_broadcast::finalization_handler::FinalizationHandler;
use crate::atomic_broadcast::network::Network;
use crate::atomic_broadcast::spawner::Spawner;
use crate::atomic_broadcast::{to_node_index, Keychain, Message};
use crate::config::ServerConfig;
use crate::consensus::process_transaction_with_dbtx;
use crate::db::{
    get_global_database_migrations, AcceptedItemKey, AcceptedItemPrefix, AcceptedTransactionKey,
    AlephUnitsPrefix, ClientConfigSignatureKey, ClientConfigSignatureShareKey,
    ClientConfigSignatureSharePrefix, SignedBlockKey, SignedBlockPrefix, GLOBAL_DATABASE_VERSION,
};
use crate::fedimint_core::encoding::Encodable;
use crate::net::api::{ConsensusApi, ExpiringCache, InvitationCodesTracker};
use crate::net::connect::{Connector, TlsTcpConnector};
use crate::net::peers::{DelayCalculator, PeerConnector, ReconnectPeerConnections};
use crate::{atomic_broadcast, LOG_CONSENSUS, LOG_CORE};

/// How many txs can be stored in memory before blocking the API
const TRANSACTION_BUFFER: usize = 1000;

pub(crate) type LatestContributionByPeer = HashMap<PeerId, u64>;

/// Runs the main server consensus loop
pub struct ConsensusServer {
    modules: ServerModuleRegistry,
    db: Database,
    connections: ReconnectPeerConnections<Message>,
    keychain: Keychain,
    client_cfg_hash: sha256::Hash,
    api_endpoints: Vec<(PeerId, SafeUrl)>,
    cfg: ServerConfig,
    submission_receiver: Receiver<ConsensusItem>,
    latest_contribution_by_peer: Arc<RwLock<LatestContributionByPeer>>,
}

impl ConsensusServer {
    /// Creates a server with real network and no delays
    pub async fn new(
        cfg: ServerConfig,
        db: Database,
        module_inits: ServerModuleInitRegistry,
        task_group: &mut TaskGroup,
    ) -> anyhow::Result<(Self, ConsensusApi)> {
        let connector: PeerConnector<Message> =
            TlsTcpConnector::new(cfg.tls_config(), cfg.local.identity).into_dyn();

        Self::new_with(
            cfg,
            db,
            module_inits,
            connector,
            DelayCalculator::PROD_DEFAULT,
            task_group,
        )
        .await
    }

    /// Creates a server that can simulate network and delays
    ///
    /// Initializes modules and runs any database migrations
    pub async fn new_with(
        cfg: ServerConfig,
        db: Database,
        module_inits: ServerModuleInitRegistry,
        connector: PeerConnector<Message>,
        delay_calculator: DelayCalculator,
        task_group: &mut TaskGroup,
    ) -> anyhow::Result<(Self, ConsensusApi)> {
        // Check the configs are valid
        cfg.validate_config(&cfg.local.identity, &module_inits)?;

        // Apply database migrations and build `ServerModuleRegistry`
        let mut modules = BTreeMap::new();

        apply_migrations(
            &db,
            "Global".to_string(),
            GLOBAL_DATABASE_VERSION,
            get_global_database_migrations(),
        )
        .await?;

        for (module_id, module_cfg) in &cfg.consensus.modules {
            let kind = module_cfg.kind.clone();
            let Some(init) = module_inits.get(&kind) else {
                bail!("Detected configuration for unsupported module id: {module_id}, kind: {kind}")
            };
            info!(target: LOG_CORE,
                module_instance_id = *module_id, kind = %kind, "Init module");

            let isolated_db = db.with_prefix_module_id(*module_id);

            apply_migrations(
                &isolated_db,
                init.module_kind().to_string(),
                init.database_version(),
                init.get_database_migrations(),
            )
            .await?;

            let module = init
                .init(
                    cfg.get_module_config(*module_id)?,
                    isolated_db,
                    task_group,
                    cfg.local.identity,
                )
                .await?;

            modules.insert(*module_id, (kind, module));
        }

        let modules = ModuleRegistry::from(modules);

        let keychain = Keychain::new(
            cfg.local.identity,
            cfg.consensus.broadcast_public_keys.clone(),
            cfg.private.broadcast_secret_key,
        );

        let (submission_sender, submission_receiver) = async_channel::bounded(TRANSACTION_BUFFER);

        // Build P2P connections for the atomic broadcast
        let (connections, peer_status_channels) = ReconnectPeerConnections::new(
            cfg.network_config(),
            delay_calculator,
            connector,
            task_group,
        )
        .await;

        // Build API that can handle requests
        let latest_contribution_by_peer = Default::default();

        let consensus_api = ConsensusApi {
            cfg: cfg.clone(),
            invitation_codes_tracker: InvitationCodesTracker::new(db.clone(), task_group).await,
            db: db.clone(),
            modules: modules.clone(),
            client_cfg: cfg.consensus.to_client_config(&module_inits)?,
            submission_sender: submission_sender.clone(),
            supported_api_versions: ServerConfig::supported_api_versions_summary(
                &cfg.consensus.modules,
                &module_inits,
            ),
            latest_contribution_by_peer: Arc::clone(&latest_contribution_by_peer),
            peer_status_channels,
            consensus_status_cache: ExpiringCache::new(Duration::from_millis(500)),
        };

        submit_module_consensus_items(
            task_group,
            db.clone(),
            modules.clone(),
            cfg.clone(),
            consensus_api.client_cfg.consensus_hash(),
            submission_sender.clone(),
        )
        .await;

        let api_endpoints: Vec<_> = cfg
            .consensus
            .api_endpoints
            .clone()
            .into_iter()
            .map(|(id, node)| (id, node.url))
            .collect();

        let consensus_server = ConsensusServer {
            connections,
            db,
            keychain,
            client_cfg_hash: consensus_api.client_cfg.consensus_hash(),
            api_endpoints,
            cfg: cfg.clone(),
            submission_receiver,
            latest_contribution_by_peer,
            modules,
        };

        Ok((consensus_server, consensus_api))
    }

    pub async fn run(&self, task_handle: TaskHandle) -> anyhow::Result<()> {
        if self.cfg.consensus.broadcast_public_keys.len() == 1 {
            self.run_single_guardian(task_handle).await
        } else {
            self.run_consensus(task_handle).await
        }
    }

    pub async fn run_single_guardian(&self, task_handle: TaskHandle) -> anyhow::Result<()> {
        assert_eq!(self.cfg.consensus.broadcast_public_keys.len(), 1);

        while !task_handle.is_shutting_down() {
            let session_index = self
                .db
                .begin_transaction()
                .await
                .find_by_prefix(&SignedBlockPrefix)
                .await
                .count()
                .await as u64;

            let mut item_index = self.build_block().await.items.len() as u64;

            let session_start_time = std::time::Instant::now();

            while let Ok(item) = self.submission_receiver.recv().await {
                if self
                    .process_consensus_item(
                        session_index,
                        item_index,
                        item,
                        self.cfg.local.identity,
                    )
                    .await
                    .is_ok()
                {
                    item_index += 1;
                }

                // we rely on the module consensus items to notice the timeout
                if session_start_time.elapsed() > Duration::from_secs(60) {
                    break;
                }
            }

            let block = self.build_block().await;
            let header = block.header(session_index);
            let signature = self.keychain.sign(&header);
            let signatures = BTreeMap::from_iter([(self.cfg.local.identity, signature)]);

            self.complete_session(session_index, SignedBlock { block, signatures })
                .await;

            info!(target: LOG_CONSENSUS, "Session completed");

            // if the submission channel is closed we are shutting down
            if self.submission_receiver.is_closed() {
                break;
            }
        }

        info!(target: LOG_CONSENSUS, "Consensus task shut down");

        Ok(())
    }

    pub async fn run_consensus(&self, task_handle: TaskHandle) -> anyhow::Result<()> {
        // We need four peers to run the atomic broadcast
        assert!(self.cfg.consensus.broadcast_public_keys.len() >= 4);

        self.confirm_consensus_config_hash().await?;

        while !task_handle.is_shutting_down() {
            let session_index = self
                .db
                .begin_transaction()
                .await
                .find_by_prefix(&SignedBlockPrefix)
                .await
                .count()
                .await as u64;

            self.run_session(session_index).await?;

            info!(target: LOG_CONSENSUS, "Session completed");
        }

        info!(target: LOG_CONSENSUS, "Consensus task shut down");

        Ok(())
    }

    async fn confirm_consensus_config_hash(&self) -> anyhow::Result<()> {
        let our_hash = self.cfg.consensus.consensus_hash();
        let federation_api = WsFederationApi::new(self.api_endpoints.clone());

        info!(target: LOG_CONSENSUS, "Waiting for peers config {our_hash}");

        loop {
            match federation_api.consensus_config_hash().await {
                Ok(consensus_hash) => {
                    if consensus_hash != our_hash {
                        bail!("Our consensus config doesn't match peers!")
                    }

                    info!(target: LOG_CONSENSUS, "Confirmed peers config {our_hash}");

                    return Ok(());
                }
                Err(e) => {
                    warn!(target: LOG_CONSENSUS, "Could not check consensus config hash: {}", OptStacktrace(e))
                }
            }

            sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn run_session(&self, session_index: u64) -> anyhow::Result<()> {
        // if all nodes are correct the session will take 45 to 60 seconds. The
        // more nodes go offline the longer the session will take to complete.
        const EXPECTED_ROUNDS_PER_SESSION: usize = 45 * 4;
        // this constant needs to be 3000 or less to guarantee that the session
        // can never reach MAX_ROUNDs.
        const EXPONENTIAL_SLOWDOWN_OFFSET: usize = 3 * EXPECTED_ROUNDS_PER_SESSION;
        const MAX_ROUND: u16 = 5000;
        const ROUND_DELAY: f64 = 250.0;
        const BASE: f64 = 1.01;

        // this is the minimum number of unit data that will be ordered before we reach
        // the EXPONENTIAL_SLOWDOWN_OFFSET even if f peers do not attach unit data
        let batches_per_session = EXPECTED_ROUNDS_PER_SESSION * self.keychain.peer_count();

        // In order to bound a sessions RAM consumption we need to bound its number of
        // units and therefore its number of rounds. Since we use a session to
        // create a threshold signature for the corresponding block we have to
        // guarantee that an attacker cannot exhaust our memory by preventing the
        // creation of a threshold signature, thereby keeping the session open
        // indefinitely. Hence we increase the delay between rounds exponentially
        // such that MAX_ROUND would only be reached after roughly 350 years.
        // In case of such an attack the broadcast stops ordering any items until the
        // attack subsides as not items are ordered while the signatures are collected.
        let mut delay_config = aleph_bft::default_delay_config();
        delay_config.unit_creation_delay = std::sync::Arc::new(|round_index| {
            let delay = if round_index == 0 {
                0.0
            } else {
                ROUND_DELAY
                    * BASE.powf(round_index.saturating_sub(EXPONENTIAL_SLOWDOWN_OFFSET) as f64)
            };

            Duration::from_millis(delay.round() as u64)
        });

        let config = aleph_bft::create_config(
            self.keychain.peer_count().into(),
            self.keychain.peer_id().to_usize().into(),
            session_index,
            MAX_ROUND,
            delay_config,
            Duration::from_secs(100 * 365 * 24 * 60 * 60),
        )
        .expect("Config is valid");

        // the number of units ordered in a single aleph session is bounded
        let (unit_data_sender, unit_data_receiver) = async_channel::unbounded();
        let (signature_sender, signature_receiver) = watch::channel(None);
        let (terminator_sender, terminator_receiver) = futures::channel::oneshot::channel();

        let (loader, saver) = atomic_broadcast::backup::load_session(self.db.clone()).await;

        let aleph_handle = spawn(
            "aleph run session",
            aleph_bft::run_session(
                config,
                aleph_bft::LocalIO::new(
                    DataProvider::new(self.submission_receiver.clone(), signature_receiver),
                    FinalizationHandler::new(unit_data_sender),
                    saver,
                    loader,
                ),
                Network::new(self.connections.clone()),
                self.keychain.clone(),
                Spawner::new(),
                aleph_bft_types::Terminator::create_root(terminator_receiver, "Terminator"),
            ),
        )
        .expect("some handle on non-wasm");

        let signed_block = self
            .complete_signed_block(
                session_index,
                batches_per_session,
                unit_data_receiver,
                signature_sender,
            )
            .await?;

        terminator_sender.send(()).ok();
        aleph_handle.await.ok();

        // Only call this after aleph bft has shutdown to avoid write-write conflicts
        // for the aleph bft units
        self.complete_session(session_index, signed_block).await;

        Ok(())
    }

    pub async fn complete_signed_block(
        &self,
        session_index: u64,
        batches_per_block: usize,
        unit_data_receiver: Receiver<(UnitData, PeerId)>,
        signature_sender: watch::Sender<Option<SchnorrSignature>>,
    ) -> anyhow::Result<SignedBlock> {
        let mut num_batches = 0;
        let mut item_index = 0;

        // we build a block out of the ordered batches until either we have processed
        // n_batches_per_block blocks or a signed block arrives from our peers
        while num_batches < batches_per_block {
            tokio::select! {
                unit_data = unit_data_receiver.recv() => {
                    if let (UnitData::Batch(bytes), peer) = unit_data? {
                        if let Ok(items) = Vec::<ConsensusItem>::consensus_decode(&mut bytes.as_slice(), &self.decoders()){
                            for item in items {
                                if self.process_consensus_item(
                                    session_index,
                                    item_index,
                                    item.clone(),
                                    peer
                                ).await
                                .is_ok() {
                                    item_index += 1;
                                }
                            }
                        }
                        num_batches += 1;
                    }
                },
                signed_block = self.request_signed_block(session_index) => {
                    let partial_block = self.build_block().await.items;

                    let (processed, unprocessed) = signed_block.block.items.split_at(partial_block.len());

                    assert!(processed.iter().eq(partial_block.iter()));

                    for accepted_item in unprocessed {
                        let result = self.process_consensus_item(
                            session_index,
                            item_index,
                            accepted_item.item.clone(),
                            accepted_item.peer
                        ).await;

                        assert!(result.is_ok());

                        item_index += 1;
                    }

                    return Ok(signed_block);
                }
            }
        }

        let block = self.build_block().await;
        let header = block.header(session_index);

        // we send our own signature to the data provider to be broadcasted
        signature_sender.send(Some(self.keychain.sign(&header)))?;

        let mut signatures = BTreeMap::new();

        // we collect the ordered signatures until we either obtain a threshold
        // signature or a signed block arrives from our peers
        while signatures.len() < self.keychain.threshold() {
            tokio::select! {
                unit_data = unit_data_receiver.recv() => {
                    if let (UnitData::Signature(signature), peer) = unit_data? {
                        if self.keychain.verify(&header, &signature, to_node_index(peer)){
                            // since the signature is valid the node index can be converted to a peer id
                            signatures.insert(peer, signature);
                        }
                    }
                }
                signed_block = self.request_signed_block(session_index) => {
                    // We check that the block we have created agrees with the federations consensus
                    assert!(header == signed_block.block.header(session_index));

                    return Ok(signed_block);
                }
            }
        }

        Ok(SignedBlock { block, signatures })
    }

    fn decoders(&self) -> ModuleDecoderRegistry {
        self.modules.decoder_registry()
    }

    pub async fn build_block(&self) -> Block {
        let items = self
            .db
            .begin_transaction()
            .await
            .find_by_prefix(&AcceptedItemPrefix)
            .await
            .map(|entry| entry.1)
            .collect()
            .await;

        Block { items }
    }

    pub async fn complete_session(&self, session_index: u64, signed_block: SignedBlock) {
        let mut dbtx = self.db.begin_transaction().await;

        dbtx.remove_by_prefix(&AlephUnitsPrefix).await;

        dbtx.remove_by_prefix(&AcceptedItemPrefix).await;

        if dbtx
            .insert_entry(&SignedBlockKey(session_index), &signed_block)
            .await
            .is_some()
        {
            panic!("We tried to overwrite a signed block");
        }

        dbtx.commit_tx_result()
            .await
            .expect("This is the only place where we write to this key");
    }

    pub async fn process_consensus_item(
        &self,
        session_index: u64,
        item_index: u64,
        item: ConsensusItem,
        peer: PeerId,
    ) -> anyhow::Result<()> {
        let _timing /* logs on drop */ = timing::TimeReporter::new("process_consensus_item");

        debug!("Peer {peer}: {}", super::debug::item_message(&item));

        self.latest_contribution_by_peer
            .write()
            .await
            .insert(peer, session_index);

        let mut dbtx = self.db.begin_transaction().await;

        if let Some(accepted_item) = dbtx
            .get_value(&AcceptedItemKey(item_index.to_owned()))
            .await
        {
            if accepted_item.item == item && accepted_item.peer == peer {
                return Ok(());
            }

            bail!("Consensus item was discarded before recovery");
        }

        self.process_consensus_item_with_db_transaction(&mut dbtx, item.clone(), peer)
            .await?;

        dbtx.insert_entry(&AcceptedItemKey(item_index), &AcceptedItem { item, peer })
            .await;

        let mut audit = Audit::default();

        for (module_instance_id, _, module) in self.modules.iter_modules() {
            module
                .audit(
                    &mut dbtx.dbtx_ref_with_prefix_module_id(module_instance_id),
                    &mut audit,
                    module_instance_id,
                )
                .await
        }

        if audit.net_assets().milli_sat < 0 {
            panic!("Balance sheet of the fed has gone negative, this should never happen! {audit}")
        }

        dbtx.commit_tx_result()
            .await
            .expect("Committing consensus epoch failed");

        Ok(())
    }

    async fn process_consensus_item_with_db_transaction(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        consensus_item: ConsensusItem,
        peer_id: PeerId,
    ) -> anyhow::Result<()> {
        // We rely on decoding rejecting any unknown module instance ids to avoid
        // peer-triggered panic here
        self.decoders().assert_reject_mode();

        match consensus_item {
            ConsensusItem::Module(module_item) => {
                let moduletx =
                    &mut dbtx.dbtx_ref_with_prefix_module_id(module_item.module_instance_id());

                self.modules
                    .get_expect(module_item.module_instance_id())
                    .process_consensus_item(moduletx, module_item, peer_id)
                    .await
            }
            ConsensusItem::Transaction(transaction) => {
                if dbtx
                    .get_value(&AcceptedTransactionKey(transaction.tx_hash()))
                    .await
                    .is_some()
                {
                    bail!("The transaction is already accepted");
                }

                let txid = transaction.tx_hash();
                let modules_ids = transaction
                    .outputs
                    .iter()
                    .map(|output| output.module_instance_id())
                    .collect::<Vec<_>>();

                process_transaction_with_dbtx(self.modules.clone(), dbtx, transaction).await?;

                dbtx.insert_entry(&AcceptedTransactionKey(txid), &modules_ids)
                    .await;

                Ok(())
            }
            ConsensusItem::ClientConfigSignatureShare(signature_share) => {
                if dbtx
                    .dbtx_ref()
                    .get_value(&ClientConfigSignatureKey)
                    .await
                    .is_some()
                {
                    bail!("Client config is already signed");
                }

                if dbtx
                    .get_value(&ClientConfigSignatureShareKey(peer_id))
                    .await
                    .is_some()
                {
                    bail!("Already received a valid signature share for this peer");
                }

                let pks = self.cfg.consensus.auth_pk_set.clone();

                if !pks
                    .public_key_share(peer_id.to_usize())
                    .verify(&signature_share.0, self.client_cfg_hash)
                {
                    bail!("Client config signature share is invalid");
                }

                // we have received the first valid signature share for this peer
                dbtx.insert_new_entry(&ClientConfigSignatureShareKey(peer_id), &signature_share)
                    .await;

                // collect all valid signature shares received previously
                let signature_shares = dbtx
                    .find_by_prefix(&ClientConfigSignatureSharePrefix)
                    .await
                    .map(|(key, share)| (key.0.to_usize(), share.0))
                    .collect::<Vec<_>>()
                    .await;

                if signature_shares.len() <= pks.threshold() {
                    return Ok(());
                }

                let threshold_signature = pks
                    .combine_signatures(signature_shares.iter().map(|(peer, share)| (peer, share)))
                    .expect("All signature shares are valid");

                dbtx.remove_by_prefix(&ClientConfigSignatureSharePrefix)
                    .await;

                dbtx.insert_entry(
                    &ClientConfigSignatureKey,
                    &SerdeSignature(threshold_signature),
                )
                .await;

                Ok(())
            }
        }
    }

    async fn request_signed_block(&self, index: u64) -> SignedBlock {
        let keychain = self.keychain.clone();
        let total_peers = self.keychain.peer_count();
        let decoders = self.decoders();

        let filter_map = move |response: SerdeModuleEncoding<SignedBlock>| match response
            .try_into_inner(&decoders)
        {
            Ok(signed_block) => {
                match signed_block.signatures.len() == keychain.threshold()
                    && signed_block.signatures.iter().all(|(peer_id, sig)| {
                        keychain.verify(
                            &signed_block.block.header(index),
                            sig,
                            to_node_index(*peer_id),
                        )
                    }) {
                    true => Ok(signed_block),
                    false => Err(anyhow!("Invalid signatures")),
                }
            }
            Err(error) => Err(anyhow!(error.to_string())),
        };

        let federation_api = WsFederationApi::new(self.api_endpoints.clone());

        loop {
            // we wait until we have stalled
            sleep(Duration::from_secs(5)).await;

            let result = federation_api
                .request_with_strategy(
                    FilterMap::new(filter_map.clone(), total_peers),
                    AWAIT_SIGNED_BLOCK_ENDPOINT.to_string(),
                    ApiRequestErased::new(index),
                )
                .await;

            match result {
                Ok(signed_block) => return signed_block,
                Err(error) => tracing::error!("Error while requesting signed block: {}", error),
            }
        }
    }
}

async fn submit_module_consensus_items(
    task_group: &mut TaskGroup,
    db: Database,
    modules: ServerModuleRegistry,
    cfg: ServerConfig,
    client_cfg_hash: sha256::Hash,
    submission_sender: Sender<ConsensusItem>,
) {
    task_group
        .spawn(
            "submit_module_consensus_items",
            move |task_handle| async move {
                while !task_handle.is_shutting_down() {
                    let mut dbtx = db.begin_transaction().await;

                    // We ignore any writes
                    dbtx.ignore_uncommitted();

                    let mut consensus_items = Vec::new();

                    for (instance_id, _, module) in modules.iter_modules() {
                        let items = module
                            .consensus_proposal(
                                &mut dbtx.dbtx_ref_with_prefix_module_id(instance_id),
                                instance_id,
                            )
                            .await
                            .into_iter()
                            .map(ConsensusItem::Module);

                        consensus_items.extend(items);
                    }

                    // Add a signature share for the client config hash
                    let sig = dbtx.dbtx_ref().get_value(&ClientConfigSignatureKey).await;

                    if sig.is_none() {
                        let timing = timing::TimeReporter::new("sign client config");
                        let share = cfg.private.auth_sks.0.sign(client_cfg_hash);
                        drop(timing);
                        let item =
                            ConsensusItem::ClientConfigSignatureShare(SerdeSignatureShare(share));
                        consensus_items.push(item);
                    }

                    for item in consensus_items {
                        submission_sender.send(item).await.ok();
                    }

                    sleep(Duration::from_secs(1)).await;
                }
            },
        )
        .await;
}

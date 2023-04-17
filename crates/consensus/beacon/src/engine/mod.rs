use crate::engine::metrics::Metrics;
use futures::{Future, FutureExt, StreamExt};
use reth_db::{database::Database, tables, transaction::DbTx};
use reth_interfaces::{
    blockchain_tree::{BlockStatus, BlockchainTreeEngine},
    consensus::ForkchoiceState,
    executor::Error as ExecutorError,
    sync::SyncStateUpdater,
    Error,
};
use reth_payload_builder::{PayloadBuilderAttributes, PayloadBuilderHandle};
use reth_primitives::{BlockNumber, Header, SealedBlock, H256};
use reth_rpc_types::engine::{
    EngineRpcError, ExecutionPayload, ForkchoiceUpdated, PayloadAttributes, PayloadStatus,
    PayloadStatusEnum,
};
use reth_stages::{stages::FINISH, Pipeline};
use reth_tasks::TaskSpawner;
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::{
    mpsc,
    mpsc::{UnboundedReceiver, UnboundedSender},
    oneshot,
};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::*;

mod message;
pub use message::BeaconEngineMessage;

mod error;
pub use error::{BeaconEngineError, BeaconEngineResult};

mod metrics;
mod pipeline_state;
pub use pipeline_state::PipelineState;

/// A _shareable_ beacon consensus frontend. Used to interact with the spawned beacon consensus
/// engine.
///
/// See also [`BeaconConsensusEngine`].
#[derive(Clone, Debug)]
pub struct BeaconConsensusEngineHandle {
    to_engine: UnboundedSender<BeaconEngineMessage>,
}

// === impl BeaconConsensusEngineHandle ===

impl BeaconConsensusEngineHandle {
    /// Creates a new beacon consensus engine handle.
    pub fn new(to_engine: UnboundedSender<BeaconEngineMessage>) -> Self {
        Self { to_engine }
    }

    /// Sends a new payload message to the beacon consensus engine and waits for a response.
    ///
    ///See also <https://github.com/ethereum/execution-apis/blob/8db51dcd2f4bdfbd9ad6e4a7560aac97010ad063/src/engine/specification.md#engine_newpayloadv2>
    pub async fn new_payload(
        &self,
        payload: ExecutionPayload,
    ) -> BeaconEngineResult<PayloadStatus> {
        let (tx, rx) = oneshot::channel();
        let _ = self.to_engine.send(BeaconEngineMessage::NewPayload { payload, tx });
        rx.await.map_err(|_| BeaconEngineError::EngineUnavailable)?
    }

    /// Sends a forkchoice update message to the beacon consensus engine and waits for a response.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/main/src/engine/specification.md#engine_forkchoiceupdatedv2>
    pub async fn fork_choice_updated(
        &self,
        state: ForkchoiceState,
        payload_attrs: Option<PayloadAttributes>,
    ) -> BeaconEngineResult<ForkchoiceUpdated> {
        self.send_fork_choice_updated(state, payload_attrs)
            .await
            .map_err(|_| BeaconEngineError::EngineUnavailable)?
    }

    /// Sends a forkchoice update message to the beacon consensus engine and returns the receiver to
    /// wait for a response.
    pub fn send_fork_choice_updated(
        &self,
        state: ForkchoiceState,
        payload_attrs: Option<PayloadAttributes>,
    ) -> oneshot::Receiver<BeaconEngineResult<ForkchoiceUpdated>> {
        let (tx, rx) = oneshot::channel();
        let _ = self.to_engine.send(BeaconEngineMessage::ForkchoiceUpdated {
            state,
            payload_attrs,
            tx,
        });
        rx
    }
}

/// The beacon consensus engine is the driver that switches between historical and live sync.
///
/// The beacon consensus engine is itself driven by messages from the Consensus Layer, which are
/// received by Engine API.
///
/// The consensus engine is idle until it receives the first
/// [BeaconEngineMessage::ForkchoiceUpdated] message from the CL which would initiate the sync. At
/// first, the consensus engine would run the [Pipeline] until the latest known block hash.
/// Afterwards, it would attempt to create/restore the [`BlockchainTreeEngine`] from the blocks
/// that are currently available. In case the restoration is successful, the consensus engine would
/// run in a live sync mode, which mean it would solemnly rely on the messages from Engine API to
/// construct the chain forward.
///
/// # Panics
///
/// If the future is polled more than once. Leads to undefined state.
#[must_use = "Future does nothing unless polled"]
pub struct BeaconConsensusEngine<DB, TS, U, BT>
where
    DB: Database,
    TS: TaskSpawner,
    U: SyncStateUpdater,
    BT: BlockchainTreeEngine,
{
    /// The database handle.
    db: Arc<DB>,
    /// Task spawner for spawning the pipeline.
    task_spawner: TS,
    /// The current state of the pipeline.
    /// Must always be [Some] unless the state is being reevaluated.
    /// The pipeline is used for historical sync by setting the current forkchoice head.
    pipeline_state: Option<PipelineState<DB, U>>,
    /// The blockchain tree used for live sync and reorg tracking.
    blockchain_tree: BT,
    /// The Engine API message receiver.
    engine_message_rx: UnboundedReceiverStream<BeaconEngineMessage>,
    /// A clone of the handle
    handle: BeaconConsensusEngineHandle,
    /// Current forkchoice state. The engine must receive the initial state in order to start
    /// syncing.
    forkchoice_state: Option<ForkchoiceState>,
    /// Next action that the engine should take after the pipeline finished running.
    next_action: BeaconEngineAction,
    /// Max block after which the consensus engine would terminate the sync. Used for debugging
    /// purposes.
    max_block: Option<BlockNumber>,
    /// The payload store.
    payload_builder: PayloadBuilderHandle,
    /// Consensus engine metrics.
    metrics: Metrics,
}

impl<DB, TS, U, BT> BeaconConsensusEngine<DB, TS, U, BT>
where
    DB: Database + Unpin + 'static,
    TS: TaskSpawner,
    U: SyncStateUpdater + 'static,
    BT: BlockchainTreeEngine + 'static,
{
    /// Create a new instance of the [BeaconConsensusEngine].
    pub fn new(
        db: Arc<DB>,
        task_spawner: TS,
        pipeline: Pipeline<DB, U>,
        blockchain_tree: BT,
        max_block: Option<BlockNumber>,
        payload_builder: PayloadBuilderHandle,
    ) -> (Self, BeaconConsensusEngineHandle) {
        let (to_engine, rx) = mpsc::unbounded_channel();
        Self::with_channel(
            db,
            task_spawner,
            pipeline,
            blockchain_tree,
            max_block,
            payload_builder,
            to_engine,
            rx,
        )
    }

    /// Create a new instance of the [BeaconConsensusEngine] using the given channel to configure
    /// the [BeaconEngineMessage] communication channel.
    #[allow(clippy::too_many_arguments)]
    pub fn with_channel(
        db: Arc<DB>,
        task_spawner: TS,
        pipeline: Pipeline<DB, U>,
        blockchain_tree: BT,
        max_block: Option<BlockNumber>,
        payload_builder: PayloadBuilderHandle,
        to_engine: UnboundedSender<BeaconEngineMessage>,
        rx: UnboundedReceiver<BeaconEngineMessage>,
    ) -> (Self, BeaconConsensusEngineHandle) {
        let handle = BeaconConsensusEngineHandle { to_engine };
        let this = Self {
            db,
            task_spawner,
            pipeline_state: Some(PipelineState::Idle(pipeline)),
            blockchain_tree,
            engine_message_rx: UnboundedReceiverStream::new(rx),
            handle: handle.clone(),
            forkchoice_state: None,
            next_action: BeaconEngineAction::None,
            max_block,
            payload_builder,
            metrics: Metrics::default(),
        };

        (this, handle)
    }

    /// Returns a new [`BeaconConsensusEngineHandle`] that can be cloned and shared.
    ///
    /// The [`BeaconConsensusEngineHandle`] can be used to interact with this
    /// [`BeaconConsensusEngine`]
    pub fn handle(&self) -> BeaconConsensusEngineHandle {
        self.handle.clone()
    }

    /// Returns `true` if the pipeline is currently idle.
    fn is_pipeline_idle(&self) -> bool {
        self.pipeline_state.as_ref().expect("pipeline state is set").is_idle()
    }

    /// Set next action to [BeaconEngineAction::RunPipeline] to indicate that
    /// consensus engine needs to run the pipeline as soon as it becomes available.
    fn require_pipeline_run(&mut self, target: PipelineTarget) {
        self.next_action = BeaconEngineAction::RunPipeline(target);
    }

    /// Called to resolve chain forks and ensure that the Execution layer is working with the latest
    /// valid chain.
    ///
    /// These responses should adhere to the [Engine API Spec for
    /// `engine_forkchoiceUpdated`](https://github.com/ethereum/execution-apis/blob/main/src/engine/paris.md#specification-1).
    fn on_forkchoice_updated(
        &mut self,
        state: ForkchoiceState,
        attrs: Option<PayloadAttributes>,
    ) -> Result<ForkchoiceUpdated, BeaconEngineError> {
        trace!(target: "consensus::engine", ?state, "Received new forkchoice state");
        if state.head_block_hash.is_zero() {
            return Ok(ForkchoiceUpdated::new(PayloadStatus::from_status(
                PayloadStatusEnum::Invalid {
                    validation_error: BeaconEngineError::ForkchoiceEmptyHead.to_string(),
                },
            )))
        }

        // TODO: check PoW / EIP-3675 terminal block conditions for the fork choice head
        // TODO: ensure validity of the payload (is this satisfied already?)

        let is_first_forkchoice = self.forkchoice_state.is_none();
        self.forkchoice_state = Some(state);
        let status = if self.is_pipeline_idle() {
            match self.blockchain_tree.make_canonical(&state.head_block_hash) {
                Ok(_) => {
                    let head_block_number = self
                        .get_block_number(state.head_block_hash)?
                        .expect("was canonicalized, so it exists");
                    let pipeline_min_progress =
                        FINISH.get_progress(&self.db.tx()?)?.unwrap_or_default();

                    if pipeline_min_progress < head_block_number {
                        self.require_pipeline_run(PipelineTarget::Head);
                    }

                    // get header for further validation
                    let header = self
                        .db
                        .view(|tx| tx.get::<tables::Headers>(head_block_number))??
                        .expect("was canonicalized, so it exists");

                    if let Some(attrs) = attrs {
                        return self.process_payload_attributes(attrs, header, state)
                    }

                    // TODO: most recent valid block in the branch defined by payload and its
                    // ancestors, not necessarily the head <https://github.com/paradigmxyz/reth/issues/2126>
                    PayloadStatus::new(PayloadStatusEnum::Valid, Some(state.head_block_hash))
                }
                Err(error) => {
                    warn!(target: "consensus::engine", ?error, ?state, "Error canonicalizing the head hash");
                    // If this is the first forkchoice received, start downloading from safe block
                    // hash.
                    let target = if is_first_forkchoice &&
                        !state.safe_block_hash.is_zero() &&
                        self.get_block_number(state.safe_block_hash)?.is_none()
                    {
                        PipelineTarget::Safe
                    } else {
                        PipelineTarget::Head
                    };
                    self.require_pipeline_run(target);
                    match error {
                        Error::Execution(error @ ExecutorError::BlockPreMerge { .. }) => {
                            PayloadStatus::from_status(PayloadStatusEnum::Invalid {
                                validation_error: error.to_string(),
                            })
                            .with_latest_valid_hash(H256::zero())
                        }
                        _ => PayloadStatus::from_status(PayloadStatusEnum::Syncing),
                    }
                }
            }
        } else {
            trace!(target: "consensus::engine", "Pipeline is syncing, skipping forkchoice update");
            PayloadStatus::from_status(PayloadStatusEnum::Syncing)
        };

        trace!(target: "consensus::engine", ?state, ?status, "Returning forkchoice status");
        Ok(ForkchoiceUpdated::new(status))
    }

    /// Validates the payload attributes with respect to the header and fork choice state.
    fn process_payload_attributes(
        &self,
        attrs: PayloadAttributes,
        header: Header,
        state: ForkchoiceState,
    ) -> Result<ForkchoiceUpdated, BeaconEngineError> {
        // 7. Client software MUST ensure that payloadAttributes.timestamp is
        //    greater than timestamp of a block referenced by
        //    forkchoiceState.headBlockHash. If this condition isn't held client
        //    software MUST respond with -38003: `Invalid payload attributes` and
        //    MUST NOT begin a payload build process. In such an event, the
        //    forkchoiceState update MUST NOT be rolled back.
        if attrs.timestamp <= header.timestamp.into() {
            return Ok(ForkchoiceUpdated::new(PayloadStatus::from_status(
                PayloadStatusEnum::Invalid {
                    validation_error: EngineRpcError::InvalidPayloadAttributes.to_string(),
                },
            )))
        }

        // 8. Client software MUST begin a payload build process building on top of
        //    forkchoiceState.headBlockHash and identified via buildProcessId value
        //    if payloadAttributes is not null and the forkchoice state has been
        //    updated successfully. The build process is specified in the Payload
        //    building section.
        let attributes = PayloadBuilderAttributes::new(header.parent_hash, attrs);
        // TODO(mattsse) this needs to be handled asynchronously
        let payload_id = self.payload_builder.send_new_payload(attributes);

        // Client software MUST respond to this method call in the following way:
        // {
        //      payloadStatus: {
        //          status: VALID,
        //          latestValidHash: forkchoiceState.headBlockHash,
        //          validationError: null
        //      },
        //      payloadId: buildProcessId
        // }
        //
        // if the payload is deemed VALID and the build process has begun.
        Ok(ForkchoiceUpdated::new(PayloadStatus::new(
            PayloadStatusEnum::Valid,
            Some(state.head_block_hash),
        ))
        .with_payload_id(payload_id))
    }

    /// When the Consensus layer receives a new block via the consensus gossip protocol,
    /// the transactions in the block are sent to the execution layer in the form of a
    /// [`ExecutionPayload`]. The Execution layer executes the transactions and validates the
    /// state in the block header, then passes validation data back to Consensus layer, that
    /// adds the block to the head of its own blockchain and attests to it. The block is then
    /// broadcast over the consensus p2p network in the form of a "Beacon block".
    ///
    /// These responses should adhere to the [Engine API Spec for
    /// `engine_newPayload`](https://github.com/ethereum/execution-apis/blob/main/src/engine/paris.md#specification).
    fn on_new_payload(
        &mut self,
        payload: ExecutionPayload,
    ) -> Result<PayloadStatus, reth_interfaces::Error> {
        let block_number = payload.block_number.as_u64();
        let block_hash = payload.block_hash;
        trace!(target: "consensus::engine", ?block_hash, block_number, "Received new payload");
        let block = match SealedBlock::try_from(payload) {
            Ok(block) => block,
            Err(error) => {
                error!(target: "consensus::engine", ?block_hash, block_number, ?error, "Invalid payload");
                return Ok(error.into())
            }
        };

        let status = if self.is_pipeline_idle() {
            let block_hash = block.hash;
            match self.blockchain_tree.insert_block(block) {
                Ok(status) => {
                    let latest_valid_hash =
                        matches!(status, BlockStatus::Valid).then_some(block_hash);
                    let status = match status {
                        BlockStatus::Valid => PayloadStatusEnum::Valid,
                        BlockStatus::Accepted => PayloadStatusEnum::Accepted,
                        BlockStatus::Disconnected => PayloadStatusEnum::Syncing,
                    };
                    PayloadStatus::new(status, latest_valid_hash)
                }
                Err(error) => {
                    let latest_valid_hash =
                        matches!(error, Error::Execution(ExecutorError::BlockPreMerge { .. }))
                            .then_some(H256::zero());
                    let status = match error {
                        Error::Execution(ExecutorError::PendingBlockIsInFuture { .. }) => {
                            PayloadStatusEnum::Syncing
                        }
                        error => PayloadStatusEnum::Invalid { validation_error: error.to_string() },
                    };
                    PayloadStatus::new(status, latest_valid_hash)
                }
            }
        } else {
            PayloadStatus::from_status(PayloadStatusEnum::Syncing)
        };
        trace!(target: "consensus::engine", ?block_hash, block_number, ?status, "Returning payload status");
        Ok(status)
    }

    /// Returns the next pipeline state depending on the current value of the next action.
    /// Resets the next action to the default value.
    fn next_pipeline_state(
        &mut self,
        pipeline: Pipeline<DB, U>,
        forkchoice_state: ForkchoiceState,
    ) -> PipelineState<DB, U> {
        let next_action = std::mem::take(&mut self.next_action);
        if let BeaconEngineAction::RunPipeline(target) = next_action {
            self.metrics.pipeline_runs.increment(1);
            let tip = match target {
                PipelineTarget::Head => forkchoice_state.head_block_hash,
                PipelineTarget::Safe => forkchoice_state.safe_block_hash,
            };
            trace!(target: "consensus::engine", ?tip, "Starting the pipeline");
            let (tx, rx) = oneshot::channel();
            let db = self.db.clone();
            self.task_spawner.spawn_critical_blocking(
                "pipeline",
                Box::pin(async move {
                    let result = pipeline.run_as_fut(db, tip).await;
                    let _ = tx.send(result);
                }),
            );
            PipelineState::Running(rx)
        } else {
            PipelineState::Idle(pipeline)
        }
    }

    /// Attempt to restore the tree with the finalized block number.
    /// If the finalized block is missing from the database, trigger the pipeline run.
    fn restore_tree_if_possible(
        &mut self,
        state: ForkchoiceState,
    ) -> Result<(), reth_interfaces::Error> {
        let needs_pipeline_run = match self.get_block_number(state.finalized_block_hash)? {
            Some(number) => {
                // Attempt to restore the tree.
                self.blockchain_tree.restore_canonical_hashes(number)?;

                // After restoring the tree, check if the head block is missing.
                self.db
                    .view(|tx| tx.get::<tables::HeaderNumbers>(state.head_block_hash))??
                    .is_none()
            }
            None => true,
        };
        if needs_pipeline_run {
            self.require_pipeline_run(PipelineTarget::Head);
        }
        Ok(())
    }

    /// Check if the engine reached max block as specified by `max_block` parameter.
    fn has_reached_max_block(&self, progress: BlockNumber) -> bool {
        if self.max_block.map_or(false, |target| progress >= target) {
            trace!(
                target: "consensus::engine",
                ?progress,
                max_block = ?self.max_block,
                "Consensus engine reached max block."
            );
            true
        } else {
            false
        }
    }

    /// Retrieve the block number for the given block hash.
    fn get_block_number(&self, hash: H256) -> Result<Option<BlockNumber>, reth_interfaces::Error> {
        Ok(self.db.view(|tx| tx.get::<tables::HeaderNumbers>(hash))??)
    }
}

/// On initialization, the consensus engine will poll the message receiver and return
/// [Poll::Pending] until the first forkchoice update message is received.
///
/// As soon as the consensus engine receives the first forkchoice updated message and updates the
/// local forkchoice state, it will launch the pipeline to sync to the head hash.
/// While the pipeline is syncing, the consensus engine will keep processing messages from the
/// receiver and forwarding them to the blockchain tree.
impl<DB, TS, U, BT> Future for BeaconConsensusEngine<DB, TS, U, BT>
where
    DB: Database + Unpin + 'static,
    TS: TaskSpawner + Unpin,
    U: SyncStateUpdater + Unpin + 'static,
    BT: BlockchainTreeEngine + Unpin + 'static,
{
    type Output = Result<(), BeaconEngineError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // Set the next pipeline state.
        loop {
            // Process all incoming messages first.
            while let Poll::Ready(Some(msg)) = this.engine_message_rx.poll_next_unpin(cx) {
                match msg {
                    BeaconEngineMessage::ForkchoiceUpdated { state, payload_attrs, tx } => {
                        this.metrics.forkchoice_updated_messages.increment(1);
                        let response = match this.on_forkchoice_updated(state, payload_attrs) {
                            Ok(response) => response,
                            Err(error) => {
                                error!(target: "consensus::engine", ?state, ?error, "Error getting forkchoice updated response");
                                return Poll::Ready(Err(error))
                            }
                        };
                        let is_valid_response =
                            matches!(response.payload_status.status, PayloadStatusEnum::Valid);
                        let _ = tx.send(Ok(response));

                        // Terminate the sync early if it's reached the maximum user
                        // configured block.
                        if is_valid_response {
                            let tip_number = this.blockchain_tree.canonical_tip().number;
                            if this.has_reached_max_block(tip_number) {
                                return Poll::Ready(Ok(()))
                            }
                        }
                    }
                    BeaconEngineMessage::NewPayload { payload, tx } => {
                        this.metrics.new_payload_messages.increment(1);
                        let response = match this.on_new_payload(payload) {
                            Ok(response) => response,
                            Err(error) => {
                                error!(target: "consensus::engine", ?error, "Error getting new payload response");
                                return Poll::Ready(Err(error.into()))
                            }
                        };
                        let _ = tx.send(Ok(response));
                    }
                }
            }

            // Lookup the forkchoice state. We can't launch the pipeline without the tip.
            let forkchoice_state = match &this.forkchoice_state {
                Some(state) => *state,
                None => return Poll::Pending,
            };

            let next_state = match this.pipeline_state.take().expect("pipeline state is set") {
                PipelineState::Running(mut fut) => {
                    match fut.poll_unpin(cx) {
                        Poll::Ready(Ok((pipeline, result))) => {
                            if let Err(error) = result {
                                return Poll::Ready(Err(error.into()))
                            }

                            match result {
                                Ok(ctrl) => {
                                    if ctrl.is_unwind() {
                                        this.require_pipeline_run(PipelineTarget::Head);
                                    } else {
                                        // Terminate the sync early if it's reached the maximum user
                                        // configured block.
                                        let minimum_pipeline_progress =
                                            pipeline.minimum_progress().unwrap_or_default();
                                        if this.has_reached_max_block(minimum_pipeline_progress) {
                                            return Poll::Ready(Ok(()))
                                        }
                                    }
                                }
                                // Any pipeline error at this point is fatal.
                                Err(error) => return Poll::Ready(Err(error.into())),
                            };

                            // Update the state and hashes of the blockchain tree if possible
                            if let Err(error) = this.restore_tree_if_possible(forkchoice_state) {
                                error!(target: "consensus::engine", ?error, "Error restoring blockchain tree");
                                return Poll::Ready(Err(error.into()))
                            }

                            // Get next pipeline state.
                            this.next_pipeline_state(pipeline, forkchoice_state)
                        }
                        Poll::Ready(Err(error)) => {
                            error!(target: "consensus::engine", ?error, "Failed to receive pipeline result");
                            return Poll::Ready(Err(BeaconEngineError::PipelineChannelClosed))
                        }
                        Poll::Pending => {
                            this.pipeline_state = Some(PipelineState::Running(fut));
                            return Poll::Pending
                        }
                    }
                }
                PipelineState::Idle(pipeline) => {
                    this.next_pipeline_state(pipeline, forkchoice_state)
                }
            };
            this.pipeline_state = Some(next_state);

            // If the pipeline is idle, break from the loop.
            if this.is_pipeline_idle() {
                return Poll::Pending
            }
        }
    }
}

/// Denotes the next action that the [BeaconConsensusEngine] should take.
#[derive(Debug, Default)]
enum BeaconEngineAction {
    #[default]
    None,
    /// Contains the type of target hash to pass to the pipeline
    RunPipeline(PipelineTarget),
}

/// The target hash to pass to the pipeline.
#[derive(Debug, Default)]
enum PipelineTarget {
    /// Corresponds to the head block hash.
    #[default]
    Head,
    /// Corresponds to the safe block hash.
    Safe,
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use reth_db::mdbx::{test_utils::create_test_rw_db, Env, WriteMap};
    use reth_executor::{
        blockchain_tree::{
            config::BlockchainTreeConfig, externals::TreeExternals, BlockchainTree,
            ShareableBlockchainTree,
        },
        post_state::PostState,
        test_utils::TestExecutorFactory,
    };
    use reth_interfaces::{sync::NoopSyncStateUpdate, test_utils::TestConsensus};
    use reth_payload_builder::test_utils::spawn_test_payload_service;
    use reth_primitives::{ChainSpec, ChainSpecBuilder, SealedBlockWithSenders, H256, MAINNET};
    use reth_provider::Transaction;
    use reth_stages::{test_utils::TestStages, ExecOutput, PipelineError, StageError};
    use reth_tasks::TokioTaskExecutor;
    use std::{collections::VecDeque, time::Duration};
    use tokio::sync::{
        oneshot::{self, error::TryRecvError},
        watch,
    };

    type TestBeaconConsensusEngine = BeaconConsensusEngine<
        Env<WriteMap>,
        TokioTaskExecutor,
        NoopSyncStateUpdate,
        ShareableBlockchainTree<Arc<Env<WriteMap>>, TestConsensus, TestExecutorFactory>,
    >;

    struct TestEnv<DB> {
        db: Arc<DB>,
        // Keep the tip receiver around, so it's not dropped.
        #[allow(dead_code)]
        tip_rx: watch::Receiver<H256>,
        engine_handle: BeaconConsensusEngineHandle,
    }

    impl<DB> TestEnv<DB> {
        fn new(
            db: Arc<DB>,
            tip_rx: watch::Receiver<H256>,
            engine_handle: BeaconConsensusEngineHandle,
        ) -> Self {
            Self { db, tip_rx, engine_handle }
        }

        async fn send_new_payload(
            &self,
            payload: ExecutionPayload,
        ) -> BeaconEngineResult<PayloadStatus> {
            self.engine_handle.new_payload(payload).await
        }

        /// Sends the `ExecutionPayload` message to the consensus engine and retries if the engine
        /// is syncing.
        async fn send_new_payload_retry_on_syncing(
            &self,
            payload: ExecutionPayload,
        ) -> BeaconEngineResult<PayloadStatus> {
            loop {
                let result = self.send_new_payload(payload.clone()).await?;
                if !result.is_syncing() {
                    return Ok(result)
                }
            }
        }

        async fn send_forkchoice_updated(
            &self,
            state: ForkchoiceState,
        ) -> BeaconEngineResult<ForkchoiceUpdated> {
            self.engine_handle.fork_choice_updated(state, None).await
        }

        /// Sends the `ForkchoiceUpdated` message to the consensus engine and retries if the engine
        /// is syncing.
        async fn send_forkchoice_retry_on_syncing(
            &self,
            state: ForkchoiceState,
        ) -> BeaconEngineResult<ForkchoiceUpdated> {
            loop {
                let result = self.engine_handle.fork_choice_updated(state, None).await?;
                if !result.is_syncing() {
                    return Ok(result)
                }
            }
        }
    }

    fn setup_consensus_engine(
        chain_spec: Arc<ChainSpec>,
        pipeline_exec_outputs: VecDeque<Result<ExecOutput, StageError>>,
        executor_results: Vec<PostState>,
    ) -> (TestBeaconConsensusEngine, TestEnv<Env<WriteMap>>) {
        reth_tracing::init_test_tracing();
        let db = create_test_rw_db();
        let consensus = TestConsensus::default();
        let payload_builder = spawn_test_payload_service();

        let executor_factory = TestExecutorFactory::new(chain_spec.clone());
        executor_factory.extend(executor_results);

        // Setup pipeline
        let (tip_tx, tip_rx) = watch::channel(H256::default());
        let pipeline = Pipeline::builder()
            .add_stages(TestStages::new(pipeline_exec_outputs, Default::default()))
            .with_tip_sender(tip_tx)
            .build();

        // Setup blockchain tree
        let externals = TreeExternals::new(db.clone(), consensus, executor_factory, chain_spec);
        let config = BlockchainTreeConfig::new(1, 2, 3);
        let (canon_state_notification_sender, _) = tokio::sync::broadcast::channel(3);
        let tree = ShareableBlockchainTree::new(
            BlockchainTree::new(externals, canon_state_notification_sender, config)
                .expect("failed to create tree"),
        );
        let (engine, handle) = BeaconConsensusEngine::new(
            db.clone(),
            TokioTaskExecutor::default(),
            pipeline,
            tree,
            None,
            payload_builder,
        );

        (engine, TestEnv::new(db, tip_rx, handle))
    }

    fn spawn_consensus_engine(
        engine: TestBeaconConsensusEngine,
    ) -> oneshot::Receiver<Result<(), BeaconEngineError>> {
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let result = engine.await;
            tx.send(result).expect("failed to forward consensus engine result");
        });
        rx
    }

    // Pipeline error is propagated.
    #[tokio::test]
    async fn pipeline_error_is_propagated() {
        let chain_spec = Arc::new(
            ChainSpecBuilder::default()
                .chain(MAINNET.chain)
                .genesis(MAINNET.genesis.clone())
                .paris_activated()
                .build(),
        );
        let (consensus_engine, env) = setup_consensus_engine(
            chain_spec,
            VecDeque::from([Err(StageError::ChannelClosed)]),
            Vec::default(),
        );
        let res = spawn_consensus_engine(consensus_engine);

        let _ = env
            .send_forkchoice_updated(ForkchoiceState {
                head_block_hash: H256::random(),
                ..Default::default()
            })
            .await;
        assert_matches!(
            res.await,
            Ok(Err(BeaconEngineError::Pipeline(n))) if matches!(*n.as_ref(),PipelineError::Stage(StageError::ChannelClosed))
        );
    }

    // Test that the consensus engine is idle until first forkchoice updated is received.
    #[tokio::test]
    async fn is_idle_until_forkchoice_is_set() {
        let chain_spec = Arc::new(
            ChainSpecBuilder::default()
                .chain(MAINNET.chain)
                .genesis(MAINNET.genesis.clone())
                .paris_activated()
                .build(),
        );
        let (consensus_engine, env) = setup_consensus_engine(
            chain_spec,
            VecDeque::from([Err(StageError::ChannelClosed)]),
            Vec::default(),
        );
        let mut rx = spawn_consensus_engine(consensus_engine);

        // consensus engine is idle
        std::thread::sleep(Duration::from_millis(100));
        assert_matches!(rx.try_recv(), Err(TryRecvError::Empty));

        // consensus engine is still idle
        let _ = env.send_new_payload(SealedBlock::default().into()).await;
        assert_matches!(rx.try_recv(), Err(TryRecvError::Empty));

        // consensus engine receives a forkchoice state and triggers the pipeline
        let _ = env
            .send_forkchoice_updated(ForkchoiceState {
                head_block_hash: H256::random(),
                ..Default::default()
            })
            .await;
        assert_matches!(
            rx.await,
            Ok(Err(BeaconEngineError::Pipeline(n))) if matches!(*n.as_ref(),PipelineError::Stage(StageError::ChannelClosed))
        );
    }

    // Test that the consensus engine runs the pipeline again if the tree cannot be restored.
    // The consensus engine will propagate the second result (error) only if it runs the pipeline
    // for the second time.
    #[tokio::test]
    async fn runs_pipeline_again_if_tree_not_restored() {
        let chain_spec = Arc::new(
            ChainSpecBuilder::default()
                .chain(MAINNET.chain)
                .genesis(MAINNET.genesis.clone())
                .paris_activated()
                .build(),
        );
        let (consensus_engine, env) = setup_consensus_engine(
            chain_spec,
            VecDeque::from([
                Ok(ExecOutput { stage_progress: 1, done: true }),
                Err(StageError::ChannelClosed),
            ]),
            Vec::default(),
        );
        let rx = spawn_consensus_engine(consensus_engine);

        let _ = env
            .send_forkchoice_updated(ForkchoiceState {
                head_block_hash: H256::random(),
                ..Default::default()
            })
            .await;

        assert_matches!(
            rx.await,
            Ok(Err(BeaconEngineError::Pipeline(n)))  if matches!(*n.as_ref(),PipelineError::Stage(StageError::ChannelClosed))
        );
    }

    #[tokio::test]
    async fn terminates_upon_reaching_max_block() {
        let max_block = 1000;
        let chain_spec = Arc::new(
            ChainSpecBuilder::default()
                .chain(MAINNET.chain)
                .genesis(MAINNET.genesis.clone())
                .paris_activated()
                .build(),
        );
        let (mut consensus_engine, env) = setup_consensus_engine(
            chain_spec,
            VecDeque::from([Ok(ExecOutput { stage_progress: max_block, done: true })]),
            Vec::default(),
        );
        consensus_engine.max_block = Some(max_block);
        let rx = spawn_consensus_engine(consensus_engine);

        let _ = env
            .send_forkchoice_updated(ForkchoiceState {
                head_block_hash: H256::random(),
                ..Default::default()
            })
            .await;
        assert_matches!(rx.await, Ok(Ok(())));
    }

    fn insert_blocks<'a, DB: Database>(db: &DB, mut blocks: impl Iterator<Item = &'a SealedBlock>) {
        let mut transaction = Transaction::new(db).unwrap();
        blocks
            .try_for_each(|b| {
                transaction
                    .insert_block(SealedBlockWithSenders::new(b.clone(), Vec::default()).unwrap())
            })
            .expect("failed to insert");
        transaction.commit().unwrap();
    }

    mod fork_choice_updated {
        use super::*;
        use reth_interfaces::test_utils::generators::random_block;

        #[tokio::test]
        async fn empty_head() {
            let chain_spec = Arc::new(
                ChainSpecBuilder::default()
                    .chain(MAINNET.chain)
                    .genesis(MAINNET.genesis.clone())
                    .paris_activated()
                    .build(),
            );
            let (consensus_engine, env) = setup_consensus_engine(
                chain_spec,
                VecDeque::from([Ok(ExecOutput { done: true, stage_progress: 0 })]),
                Vec::default(),
            );

            let mut engine_rx = spawn_consensus_engine(consensus_engine);

            let res = env.send_forkchoice_updated(ForkchoiceState::default()).await;
            let expected_result = ForkchoiceUpdated::from_status(PayloadStatusEnum::Invalid {
                validation_error: BeaconEngineError::ForkchoiceEmptyHead.to_string(),
            });
            assert_matches!(res, Ok(result) => assert_eq!(result, expected_result));

            assert_matches!(engine_rx.try_recv(), Err(TryRecvError::Empty));
        }

        #[tokio::test]
        async fn valid_forkchoice() {
            let chain_spec = Arc::new(
                ChainSpecBuilder::default()
                    .chain(MAINNET.chain)
                    .genesis(MAINNET.genesis.clone())
                    .paris_activated()
                    .build(),
            );
            let (consensus_engine, env) = setup_consensus_engine(
                chain_spec,
                VecDeque::from([Ok(ExecOutput { done: true, stage_progress: 0 })]),
                Vec::default(),
            );

            let genesis = random_block(0, None, None, Some(0));
            let block1 = random_block(1, Some(genesis.hash), None, Some(0));
            insert_blocks(env.db.as_ref(), [&genesis, &block1].into_iter());
            env.db.update(|tx| FINISH.save_progress(tx, block1.number)).unwrap().unwrap();

            let mut engine_rx = spawn_consensus_engine(consensus_engine);

            let forkchoice = ForkchoiceState {
                head_block_hash: block1.hash,
                finalized_block_hash: block1.hash,
                ..Default::default()
            };

            let rx_invalid = env.send_forkchoice_updated(forkchoice);
            let expected_result = ForkchoiceUpdated::from_status(PayloadStatusEnum::Syncing);
            assert_matches!(rx_invalid.await, Ok(result) => assert_eq!(result, expected_result));

            let result = env.send_forkchoice_retry_on_syncing(forkchoice).await.unwrap();
            let expected_result = ForkchoiceUpdated::new(PayloadStatus::new(
                PayloadStatusEnum::Valid,
                Some(block1.hash),
            ));
            assert_eq!(result, expected_result);
            assert_matches!(engine_rx.try_recv(), Err(TryRecvError::Empty));
        }

        #[tokio::test]
        async fn unknown_head_hash() {
            let chain_spec = Arc::new(
                ChainSpecBuilder::default()
                    .chain(MAINNET.chain)
                    .genesis(MAINNET.genesis.clone())
                    .paris_activated()
                    .build(),
            );
            let (consensus_engine, env) = setup_consensus_engine(
                chain_spec,
                VecDeque::from([
                    Ok(ExecOutput { done: true, stage_progress: 0 }),
                    Ok(ExecOutput { done: true, stage_progress: 0 }),
                ]),
                Vec::default(),
            );

            let genesis = random_block(0, None, None, Some(0));
            let block1 = random_block(1, Some(genesis.hash), None, Some(0));
            insert_blocks(env.db.as_ref(), [&genesis, &block1].into_iter());

            let mut engine_rx = spawn_consensus_engine(consensus_engine);

            let next_head = random_block(2, Some(block1.hash), None, Some(0));
            let next_forkchoice_state = ForkchoiceState {
                head_block_hash: next_head.hash,
                finalized_block_hash: block1.hash,
                ..Default::default()
            };

            let invalid_rx = env.send_forkchoice_updated(next_forkchoice_state);

            // Insert next head immediately after sending forkchoice update
            insert_blocks(env.db.as_ref(), [&next_head].into_iter());

            let expected_result = ForkchoiceUpdated::from_status(PayloadStatusEnum::Syncing);
            assert_matches!(invalid_rx.await, Ok(result) => assert_eq!(result, expected_result));

            let result = env.send_forkchoice_retry_on_syncing(next_forkchoice_state).await.unwrap();
            let expected_result = ForkchoiceUpdated::from_status(PayloadStatusEnum::Valid)
                .with_latest_valid_hash(next_head.hash);
            assert_eq!(result, expected_result);

            assert_matches!(engine_rx.try_recv(), Err(TryRecvError::Empty));
        }

        #[tokio::test]
        async fn unknown_finalized_hash() {
            let chain_spec = Arc::new(
                ChainSpecBuilder::default()
                    .chain(MAINNET.chain)
                    .genesis(MAINNET.genesis.clone())
                    .paris_activated()
                    .build(),
            );
            let (consensus_engine, env) = setup_consensus_engine(
                chain_spec,
                VecDeque::from([Ok(ExecOutput { done: true, stage_progress: 0 })]),
                Vec::default(),
            );

            let genesis = random_block(0, None, None, Some(0));
            let block1 = random_block(1, Some(genesis.hash), None, Some(0));
            insert_blocks(env.db.as_ref(), [&genesis, &block1].into_iter());

            let engine = spawn_consensus_engine(consensus_engine);

            let res = env
                .send_forkchoice_updated(ForkchoiceState {
                    head_block_hash: H256::random(),
                    finalized_block_hash: block1.hash,
                    ..Default::default()
                })
                .await;
            let expected_result = ForkchoiceUpdated::from_status(PayloadStatusEnum::Syncing);
            assert_matches!(res, Ok(result) => assert_eq!(result, expected_result));
            drop(engine);
        }

        #[tokio::test]
        async fn forkchoice_updated_invalid_pow() {
            let chain_spec = Arc::new(
                ChainSpecBuilder::default()
                    .chain(MAINNET.chain)
                    .genesis(MAINNET.genesis.clone())
                    .london_activated()
                    .build(),
            );
            let (consensus_engine, env) = setup_consensus_engine(
                chain_spec,
                VecDeque::from([
                    Ok(ExecOutput { done: true, stage_progress: 0 }),
                    Ok(ExecOutput { done: true, stage_progress: 0 }),
                ]),
                Vec::default(),
            );

            let genesis = random_block(0, None, None, Some(0));
            let block1 = random_block(1, Some(genesis.hash), None, Some(0));

            insert_blocks(env.db.as_ref(), [&genesis, &block1].into_iter());

            let _engine = spawn_consensus_engine(consensus_engine);

            let res = env
                .send_forkchoice_updated(ForkchoiceState {
                    head_block_hash: block1.hash,
                    finalized_block_hash: block1.hash,
                    ..Default::default()
                })
                .await;
            let expected_result = ForkchoiceUpdated::from_status(PayloadStatusEnum::Syncing);
            assert_matches!(res, Ok(result) => assert_eq!(result, expected_result));

            let result = env
                .send_forkchoice_retry_on_syncing(ForkchoiceState {
                    head_block_hash: block1.hash,
                    finalized_block_hash: block1.hash,
                    ..Default::default()
                })
                .await
                .unwrap();
            let expected_result = ForkchoiceUpdated::from_status(PayloadStatusEnum::Invalid {
                validation_error: ExecutorError::BlockPreMerge { hash: block1.hash }.to_string(),
            })
            .with_latest_valid_hash(H256::zero());

            assert_eq!(result, expected_result);
        }
    }

    mod new_payload {
        use super::*;
        use reth_interfaces::{
            executor::Error as ExecutorError, test_utils::generators::random_block,
        };
        use reth_primitives::{Hardfork, U256};
        use reth_provider::test_utils::blocks::BlockChainTestData;

        #[tokio::test]
        async fn new_payload_before_forkchoice() {
            let chain_spec = Arc::new(
                ChainSpecBuilder::default()
                    .chain(MAINNET.chain)
                    .genesis(MAINNET.genesis.clone())
                    .paris_activated()
                    .build(),
            );
            let (consensus_engine, env) = setup_consensus_engine(
                chain_spec,
                VecDeque::from([Ok(ExecOutput { done: true, stage_progress: 0 })]),
                Vec::default(),
            );

            let mut engine_rx = spawn_consensus_engine(consensus_engine);

            // Send new payload
            let res = env.send_new_payload(random_block(0, None, None, Some(0)).into()).await;
            // Invalid, because this is a genesis block
            assert_matches!(res, Ok(result) => assert_matches!(result.status, PayloadStatusEnum::Invalid { .. }));

            // Send new payload
            let res = env.send_new_payload(random_block(1, None, None, Some(0)).into()).await;
            let expected_result = PayloadStatus::from_status(PayloadStatusEnum::Syncing);
            assert_matches!(res, Ok(result) => assert_eq!(result, expected_result));

            assert_matches!(engine_rx.try_recv(), Err(TryRecvError::Empty));
        }

        #[tokio::test]
        async fn payload_known() {
            let chain_spec = Arc::new(
                ChainSpecBuilder::default()
                    .chain(MAINNET.chain)
                    .genesis(MAINNET.genesis.clone())
                    .paris_activated()
                    .build(),
            );
            let (consensus_engine, env) = setup_consensus_engine(
                chain_spec,
                VecDeque::from([Ok(ExecOutput { done: true, stage_progress: 0 })]),
                Vec::default(),
            );

            let genesis = random_block(0, None, None, Some(0));
            let block1 = random_block(1, Some(genesis.hash), None, Some(0));
            let block2 = random_block(2, Some(block1.hash), None, Some(0));
            insert_blocks(env.db.as_ref(), [&genesis, &block1, &block2].into_iter());

            let mut engine_rx = spawn_consensus_engine(consensus_engine);

            // Send forkchoice
            let res = env
                .send_forkchoice_updated(ForkchoiceState {
                    head_block_hash: block1.hash,
                    finalized_block_hash: block1.hash,
                    ..Default::default()
                })
                .await;
            let expected_result =
                ForkchoiceUpdated::new(PayloadStatus::from_status(PayloadStatusEnum::Syncing));
            assert_matches!(res, Ok(result) => assert_eq!(result, expected_result));

            // Send new payload
            let result =
                env.send_new_payload_retry_on_syncing(block2.clone().into()).await.unwrap();
            let expected_result = PayloadStatus::from_status(PayloadStatusEnum::Valid)
                .with_latest_valid_hash(block2.hash);
            assert_eq!(result, expected_result);
            assert_matches!(engine_rx.try_recv(), Err(TryRecvError::Empty));
        }

        #[tokio::test]
        async fn payload_parent_unknown() {
            let chain_spec = Arc::new(
                ChainSpecBuilder::default()
                    .chain(MAINNET.chain)
                    .genesis(MAINNET.genesis.clone())
                    .paris_activated()
                    .build(),
            );
            let (consensus_engine, env) = setup_consensus_engine(
                chain_spec,
                VecDeque::from([Ok(ExecOutput { done: true, stage_progress: 0 })]),
                Vec::default(),
            );

            let genesis = random_block(0, None, None, Some(0));

            insert_blocks(env.db.as_ref(), [&genesis].into_iter());

            let mut engine_rx = spawn_consensus_engine(consensus_engine);

            // Send forkchoice
            let res = env
                .send_forkchoice_updated(ForkchoiceState {
                    head_block_hash: genesis.hash,
                    finalized_block_hash: genesis.hash,
                    ..Default::default()
                })
                .await;
            let expected_result =
                ForkchoiceUpdated::new(PayloadStatus::from_status(PayloadStatusEnum::Syncing));
            assert_matches!(res, Ok(result) => assert_eq!(result, expected_result));

            // Send new payload
            let block = random_block(2, Some(H256::random()), None, Some(0));
            let res = env.send_new_payload(block.into()).await;
            let expected_result = PayloadStatus::from_status(PayloadStatusEnum::Syncing);
            assert_matches!(res, Ok(result) => assert_eq!(result, expected_result));

            assert_matches!(engine_rx.try_recv(), Err(TryRecvError::Empty));
        }

        #[tokio::test]
        async fn payload_pre_merge() {
            let data = BlockChainTestData::default();
            let mut block1 = data.blocks[0].0.block.clone();
            block1.header.difficulty = MAINNET.fork(Hardfork::Paris).ttd().unwrap() - U256::from(1);
            block1 = block1.unseal().seal_slow();
            let (block2, exec_result2) = data.blocks[1].clone();
            let mut block2 = block2.block;
            block2.withdrawals = None;
            block2.header.parent_hash = block1.hash;
            block2.header.base_fee_per_gas = Some(100);
            block2.header.difficulty = U256::ZERO;
            block2 = block2.unseal().seal_slow();

            let chain_spec = Arc::new(
                ChainSpecBuilder::default()
                    .chain(MAINNET.chain)
                    .genesis(MAINNET.genesis.clone())
                    .london_activated()
                    .build(),
            );
            let (consensus_engine, env) = setup_consensus_engine(
                chain_spec,
                VecDeque::from([Ok(ExecOutput { done: true, stage_progress: 0 })]),
                Vec::from([exec_result2]),
            );

            insert_blocks(env.db.as_ref(), [&data.genesis, &block1].into_iter());

            let mut engine_rx = spawn_consensus_engine(consensus_engine);

            // Send forkchoice
            let res = env
                .send_forkchoice_updated(ForkchoiceState {
                    head_block_hash: block1.hash,
                    finalized_block_hash: block1.hash,
                    ..Default::default()
                })
                .await;
            let expected_result =
                ForkchoiceUpdated::new(PayloadStatus::from_status(PayloadStatusEnum::Syncing));
            assert_matches!(res, Ok(result) => assert_eq!(result, expected_result));

            // Send new payload
            let result =
                env.send_new_payload_retry_on_syncing(block2.clone().into()).await.unwrap();

            let expected_result = PayloadStatus::from_status(PayloadStatusEnum::Invalid {
                validation_error: ExecutorError::BlockPreMerge { hash: block2.hash }.to_string(),
            })
            .with_latest_valid_hash(H256::zero());
            assert_eq!(result, expected_result);

            assert_matches!(engine_rx.try_recv(), Err(TryRecvError::Empty));
        }
    }
}

use std::future::ready;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;
use std::vec;

use apollo_batcher_types::batcher_types::{
    CentralObjects,
    DecisionReachedResponse,
    GetProposalContent,
    GetProposalContentResponse,
    ProposalCommitment,
    ProposalStatus,
    ProposeBlockInput,
    SendProposalContent,
    SendProposalContentInput,
    SendProposalContentResponse,
    ValidateBlockInput,
};
use apollo_batcher_types::communication::{BatcherClientError, MockBatcherClient};
use apollo_batcher_types::errors::BatcherError;
use apollo_class_manager_types::transaction_converter::{
    TransactionConverter,
    TransactionConverterTrait,
};
use apollo_class_manager_types::EmptyClassManagerClient;
use apollo_consensus::types::{ConsensusContext, Round};
use apollo_l1_gas_price_types::errors::{
    EthToStrkOracleClientError,
    L1GasPriceClientError,
    L1GasPriceProviderError,
};
use apollo_l1_gas_price_types::{
    MockEthToStrkOracleClientTrait,
    MockL1GasPriceProviderClient,
    PriceInfo,
    DEFAULT_ETH_TO_FRI_RATE,
};
use apollo_network::network_manager::test_utils::{
    mock_register_broadcast_topic,
    BroadcastNetworkMock,
    TestSubscriberChannels,
};
use apollo_network::network_manager::BroadcastTopicChannels;
use apollo_protobuf::consensus::{
    ConsensusBlockInfo,
    HeightAndRound,
    ProposalFin,
    ProposalInit,
    ProposalPart,
    TransactionBatch,
    Vote,
};
use apollo_state_sync_types::communication::MockStateSyncClient;
use chrono::{TimeZone, Utc};
use futures::channel::mpsc;
use futures::channel::oneshot::Canceled;
use futures::executor::block_on;
use futures::future::pending;
use futures::{FutureExt, SinkExt, StreamExt};
use metrics_exporter_prometheus::PrometheusBuilder;
use rstest::rstest;
use starknet_api::block::{
    BlockHash,
    BlockNumber,
    GasPrice,
    TEMP_ETH_BLOB_GAS_FEE_IN_WEI,
    TEMP_ETH_GAS_FEE_IN_WEI,
};
use starknet_api::consensus_transaction::{ConsensusTransaction, InternalConsensusTransaction};
use starknet_api::core::{ChainId, Nonce, StateDiffCommitment};
use starknet_api::data_availability::L1DataAvailabilityMode;
use starknet_api::execution_resources::GasAmount;
use starknet_api::felt;
use starknet_api::hash::PoseidonHash;
use starknet_api::state::ThinStateDiff;
use starknet_api::test_utils::invoke::{rpc_invoke_tx, InvokeTxArgs};
use starknet_types_core::felt::Felt;

use super::{DefaultClock, SequencerConsensusContextDeps};
use crate::cende::MockCendeContext;
use crate::config::ContextConfig;
use crate::metrics::CONSENSUS_L2_GAS_PRICE;
use crate::orchestrator_versioned_constants::VersionedConstants;
use crate::sequencer_consensus_context::{MockClock, SequencerConsensusContext};

const TIMEOUT: Duration = Duration::from_millis(1200);
const CHANNEL_SIZE: usize = 5000;
const NUM_VALIDATORS: u64 = 4;
const STATE_DIFF_COMMITMENT: StateDiffCommitment = StateDiffCommitment(PoseidonHash(Felt::ZERO));
const CHAIN_ID: ChainId = ChainId::Mainnet;

// In order for gas price in ETH to be greather than 0 (required) we must have large enough
// values here.
const ETH_TO_FRI_RATE: u128 = u128::pow(10, 18);

static TX_BATCH: LazyLock<Vec<ConsensusTransaction>> =
    LazyLock::new(|| (0..3).map(generate_invoke_tx).collect());

static INTERNAL_TX_BATCH: LazyLock<Vec<InternalConsensusTransaction>> = LazyLock::new(|| {
    // TODO(shahak): Use MockTransactionConverter instead.
    static TRANSACTION_CONVERTER: LazyLock<TransactionConverter> =
        LazyLock::new(|| TransactionConverter::new(Arc::new(EmptyClassManagerClient), CHAIN_ID));
    TX_BATCH
        .iter()
        .cloned()
        .map(|tx| {
            block_on(TRANSACTION_CONVERTER.convert_consensus_tx_to_internal_consensus_tx(tx))
                .unwrap()
        })
        .collect()
});

fn generate_invoke_tx(nonce: u8) -> ConsensusTransaction {
    ConsensusTransaction::RpcTransaction(rpc_invoke_tx(InvokeTxArgs {
        nonce: Nonce(felt!(nonce)),
        ..Default::default()
    }))
}

fn block_info(height: BlockNumber) -> ConsensusBlockInfo {
    let context_config = ContextConfig::default();
    ConsensusBlockInfo {
        height,
        timestamp: chrono::Utc::now().timestamp().try_into().expect("Timestamp conversion failed"),
        builder: Default::default(),
        l1_da_mode: L1DataAvailabilityMode::Blob,
        l2_gas_price_fri: VersionedConstants::latest_constants().min_gas_price,
        l1_gas_price_wei: GasPrice(TEMP_ETH_GAS_FEE_IN_WEI),
        l1_data_gas_price_wei: GasPrice(
            TEMP_ETH_BLOB_GAS_FEE_IN_WEI * context_config.l1_data_gas_price_multiplier_ppt / 1000,
        ),
        eth_to_fri_rate: ETH_TO_FRI_RATE,
    }
}
// Structs which aren't utilized but should not be dropped.
struct NetworkDependencies {
    _vote_network: BroadcastNetworkMock<Vote>,
    outbound_proposal_receiver: mpsc::Receiver<(HeightAndRound, mpsc::Receiver<ProposalPart>)>,
}

fn default_context_dependencies() -> (SequencerConsensusContextDeps, NetworkDependencies) {
    let (outbound_proposal_sender, outbound_proposal_receiver) =
        mpsc::channel::<(HeightAndRound, mpsc::Receiver<ProposalPart>)>(CHANNEL_SIZE);

    let TestSubscriberChannels { mock_network: mock_vote_network, subscriber_channels } =
        mock_register_broadcast_topic().expect("Failed to create mock network");
    let BroadcastTopicChannels { broadcast_topic_client: votes_topic_client, .. } =
        subscriber_channels;

    let sequencer_deps = SequencerConsensusContextDeps {
        class_manager_client: Arc::new(EmptyClassManagerClient),
        state_sync_client: Arc::new(MockStateSyncClient::new()),
        batcher: Arc::new(MockBatcherClient::new()),
        outbound_proposal_sender,
        vote_broadcast_client: votes_topic_client,
        cende_ambassador: Arc::new(success_cende_ammbassador()),
        eth_to_strk_oracle_client: Arc::new(dummy_eth_to_strk_oracle_client()),
        l1_gas_price_provider: Arc::new(dummy_gas_price_provider()),
        clock: Arc::new(DefaultClock::default()),
    };

    let network_dependencies =
        NetworkDependencies { _vote_network: mock_vote_network, outbound_proposal_receiver };

    (sequencer_deps, network_dependencies)
}

fn setup_with_custom_mocks(
    context_deps: SequencerConsensusContextDeps,
) -> SequencerConsensusContext {
    SequencerConsensusContext::new(
        ContextConfig {
            proposal_buffer_size: CHANNEL_SIZE,
            num_validators: NUM_VALIDATORS,
            chain_id: CHAIN_ID,
            ..Default::default()
        },
        context_deps,
    )
}

// Setup batcher client for proposal building flow.
async fn setup_batcher_for_build(batcher: &mut MockBatcherClient, block_number: BlockNumber) {
    let proposal_id = Arc::new(OnceLock::new());
    let proposal_id_clone = Arc::clone(&proposal_id);
    batcher.expect_propose_block().times(1).returning(move |input: ProposeBlockInput| {
        proposal_id_clone.set(input.proposal_id).unwrap();
        Ok(())
    });
    batcher
        .expect_start_height()
        .times(1)
        .withf(move |input| input.height == block_number)
        .return_const(Ok(()));
    let proposal_id_clone = Arc::clone(&proposal_id);
    batcher.expect_get_proposal_content().times(1).returning(move |input| {
        assert_eq!(input.proposal_id, *proposal_id_clone.get().unwrap());
        Ok(GetProposalContentResponse {
            content: GetProposalContent::Txs(INTERNAL_TX_BATCH.clone()),
        })
    });
    let proposal_id_clone = Arc::clone(&proposal_id);
    batcher.expect_get_proposal_content().times(1).returning(move |input| {
        assert_eq!(input.proposal_id, *proposal_id_clone.get().unwrap());
        Ok(GetProposalContentResponse {
            content: GetProposalContent::Finished(ProposalCommitment {
                state_diff_commitment: STATE_DIFF_COMMITMENT,
            }),
        })
    });
}

// Setup batcher client for proposal validation flow.
async fn setup_batcher_for_validate(batcher: &mut MockBatcherClient, block_number: BlockNumber) {
    let proposal_id = Arc::new(OnceLock::new());
    let proposal_id_clone = Arc::clone(&proposal_id);
    batcher.expect_validate_block().times(1).returning(move |input: ValidateBlockInput| {
        proposal_id_clone.set(input.proposal_id).unwrap();
        Ok(())
    });
    batcher
        .expect_start_height()
        .times(1)
        .withf(move |input| input.height == block_number)
        .return_const(Ok(()));
    let proposal_id_clone = Arc::clone(&proposal_id);
    batcher.expect_send_proposal_content().times(1).returning(
        move |input: SendProposalContentInput| {
            assert_eq!(input.proposal_id, *proposal_id_clone.get().unwrap());
            let SendProposalContent::Txs(txs) = input.content else {
                panic!("Expected SendProposalContent::Txs, got {:?}", input.content);
            };
            assert_eq!(txs, *INTERNAL_TX_BATCH);
            Ok(SendProposalContentResponse { response: ProposalStatus::Processing })
        },
    );
    let proposal_id_clone = Arc::clone(&proposal_id);
    batcher.expect_send_proposal_content().times(1).returning(
        move |input: SendProposalContentInput| {
            assert_eq!(input.proposal_id, *proposal_id_clone.get().unwrap());
            assert!(matches!(input.content, SendProposalContent::Finish));
            Ok(SendProposalContentResponse {
                response: ProposalStatus::Finished(ProposalCommitment {
                    state_diff_commitment: STATE_DIFF_COMMITMENT,
                }),
            })
        },
    );
}

// Returns a mock CendeContext that will return a successful write_prev_height_blob.
fn success_cende_ammbassador() -> MockCendeContext {
    let mut mock_cende = MockCendeContext::new();
    mock_cende.expect_write_prev_height_blob().return_once(|_height| tokio::spawn(ready(true)));
    mock_cende
}

fn dummy_gas_price_provider() -> MockL1GasPriceProviderClient {
    let mut l1_gas_price_provider = MockL1GasPriceProviderClient::new();
    l1_gas_price_provider.expect_get_price_info().return_const(Ok(PriceInfo {
        base_fee_per_gas: GasPrice(TEMP_ETH_GAS_FEE_IN_WEI),
        blob_fee: GasPrice(TEMP_ETH_BLOB_GAS_FEE_IN_WEI),
    }));

    l1_gas_price_provider
}

fn dummy_eth_to_strk_oracle_client() -> MockEthToStrkOracleClientTrait {
    let mut eth_to_strk_oracle_client = MockEthToStrkOracleClientTrait::new();
    eth_to_strk_oracle_client.expect_eth_to_fri_rate().returning(|_| Ok(ETH_TO_FRI_RATE));
    eth_to_strk_oracle_client
}

#[tokio::test]
async fn cancelled_proposal_aborts() {
    let mut batcher = MockBatcherClient::new();
    batcher.expect_propose_block().times(1).return_const(Ok(()));

    batcher.expect_start_height().times(1).return_const(Ok(()));
    let (default_deps, _network) = default_context_dependencies();
    let context_deps = SequencerConsensusContextDeps { batcher: Arc::new(batcher), ..default_deps };
    let mut context = setup_with_custom_mocks(context_deps);

    let fin_receiver = context.build_proposal(ProposalInit::default(), TIMEOUT).await;

    // Now we intrrupt the proposal and verify that the fin_receiever is dropped.
    context.set_height_and_round(BlockNumber(0), 1).await;

    assert_eq!(fin_receiver.await, Err(Canceled));
}

#[tokio::test]
async fn validate_proposal_success() {
    let mut batcher = MockBatcherClient::new();
    setup_batcher_for_validate(&mut batcher, BlockNumber(0)).await;
    let (default_deps, _network) = default_context_dependencies();
    let context_deps = SequencerConsensusContextDeps { batcher: Arc::new(batcher), ..default_deps };
    let mut context = setup_with_custom_mocks(context_deps);

    // Initialize the context for a specific height, starting with round 0.
    context.set_height_and_round(BlockNumber(0), 0).await;

    let (mut content_sender, content_receiver) = mpsc::channel(context.config.proposal_buffer_size);
    content_sender.send(ProposalPart::BlockInfo(block_info(BlockNumber(0)))).await.unwrap();
    content_sender
        .send(ProposalPart::Transactions(TransactionBatch { transactions: TX_BATCH.to_vec() }))
        .await
        .unwrap();
    content_sender
        .send(ProposalPart::Fin(ProposalFin {
            proposal_commitment: BlockHash(STATE_DIFF_COMMITMENT.0.0),
        }))
        .await
        .unwrap();
    let fin_receiver =
        context.validate_proposal(ProposalInit::default(), TIMEOUT, content_receiver).await;
    content_sender.close_channel();
    assert_eq!(fin_receiver.await.unwrap().0, STATE_DIFF_COMMITMENT.0.0);
}

#[tokio::test]
async fn dont_send_block_info() {
    let mut batcher = MockBatcherClient::new();
    batcher
        .expect_start_height()
        .times(1)
        .withf(|input| input.height == BlockNumber(0))
        .return_const(Ok(()));
    let (default_deps, _network) = default_context_dependencies();
    let context_deps = SequencerConsensusContextDeps { batcher: Arc::new(batcher), ..default_deps };
    let mut context = setup_with_custom_mocks(context_deps);

    // Initialize the context for a specific height, starting with round 0.
    context.set_height_and_round(BlockNumber(0), 0).await;

    let (mut content_sender, content_receiver) = mpsc::channel(context.config.proposal_buffer_size);
    let fin_receiver =
        context.validate_proposal(ProposalInit::default(), TIMEOUT, content_receiver).await;
    content_sender.close_channel();
    // No block info was sent, the proposal is invalid.
    assert!(fin_receiver.await.is_err());
}

#[tokio::test]
async fn repropose() {
    // Receive a proposal. Then re-retrieve it.
    let mut batcher = MockBatcherClient::new();
    setup_batcher_for_validate(&mut batcher, BlockNumber(0)).await;
    let (default_deps, mut network) = default_context_dependencies();
    let context_deps = SequencerConsensusContextDeps { batcher: Arc::new(batcher), ..default_deps };
    let mut context = setup_with_custom_mocks(context_deps);

    // Initialize the context for a specific height, starting with round 0.
    context.set_height_and_round(BlockNumber(0), 0).await;

    // Receive a valid proposal.
    let (mut content_sender, content_receiver) = mpsc::channel(context.config.proposal_buffer_size);
    let block_info = ProposalPart::BlockInfo(block_info(BlockNumber(0)));
    content_sender.send(block_info.clone()).await.unwrap();
    let transactions =
        ProposalPart::Transactions(TransactionBatch { transactions: TX_BATCH.to_vec() });
    content_sender.send(transactions.clone()).await.unwrap();
    let fin = ProposalPart::Fin(ProposalFin {
        proposal_commitment: BlockHash(STATE_DIFF_COMMITMENT.0.0),
    });
    content_sender.send(fin.clone()).await.unwrap();
    let fin_receiver =
        context.validate_proposal(ProposalInit::default(), TIMEOUT, content_receiver).await;
    content_sender.close_channel();
    assert_eq!(fin_receiver.await.unwrap().0, STATE_DIFF_COMMITMENT.0.0);

    let init = ProposalInit { round: 1, ..Default::default() };
    context.repropose(BlockHash(STATE_DIFF_COMMITMENT.0.0), init).await;
    let (_, mut receiver) = network.outbound_proposal_receiver.next().await.unwrap();
    assert_eq!(receiver.next().await.unwrap(), ProposalPart::Init(init));
    assert_eq!(receiver.next().await.unwrap(), block_info);
    assert_eq!(receiver.next().await.unwrap(), transactions);
    assert_eq!(receiver.next().await.unwrap(), fin);
    assert!(receiver.next().await.is_none());
}

#[tokio::test]
async fn proposals_from_different_rounds() {
    let mut batcher = MockBatcherClient::new();
    setup_batcher_for_validate(&mut batcher, BlockNumber(0)).await;
    let (default_deps, _network) = default_context_dependencies();
    let context_deps = SequencerConsensusContextDeps { batcher: Arc::new(batcher), ..default_deps };
    let mut context = setup_with_custom_mocks(context_deps);
    // Initialize the context for a specific height, starting with round 0.
    context.set_height_and_round(BlockNumber(0), 0).await;
    context.set_height_and_round(BlockNumber(0), 1).await;

    // Proposal parts sent in the proposals.
    let prop_part_txs =
        ProposalPart::Transactions(TransactionBatch { transactions: TX_BATCH.to_vec() });
    let prop_part_fin = ProposalPart::Fin(ProposalFin {
        proposal_commitment: BlockHash(STATE_DIFF_COMMITMENT.0.0),
    });

    // The proposal from the past round is ignored.
    let (mut content_sender, content_receiver) = mpsc::channel(context.config.proposal_buffer_size);
    content_sender.send(ProposalPart::BlockInfo(block_info(BlockNumber(0)))).await.unwrap();
    content_sender.send(prop_part_txs.clone()).await.unwrap();

    let mut init = ProposalInit { round: 0, ..Default::default() };
    let fin_receiver_past_round = context.validate_proposal(init, TIMEOUT, content_receiver).await;
    // No fin was sent, channel remains open.
    assert!(fin_receiver_past_round.await.is_err());

    // The proposal from the current round should be validated.
    let (mut content_sender, content_receiver) = mpsc::channel(context.config.proposal_buffer_size);
    content_sender.send(ProposalPart::BlockInfo(block_info(BlockNumber(0)))).await.unwrap();
    content_sender.send(prop_part_txs.clone()).await.unwrap();
    content_sender.send(prop_part_fin.clone()).await.unwrap();
    init.round = 1;
    let fin_receiver_curr_round = context.validate_proposal(init, TIMEOUT, content_receiver).await;
    assert_eq!(fin_receiver_curr_round.await.unwrap().0, STATE_DIFF_COMMITMENT.0.0);

    // The proposal from the future round should not be processed.
    let (mut content_sender, content_receiver) = mpsc::channel(context.config.proposal_buffer_size);
    content_sender.send(ProposalPart::BlockInfo(block_info(BlockNumber(0)))).await.unwrap();
    content_sender.send(prop_part_txs.clone()).await.unwrap();
    content_sender.send(prop_part_fin.clone()).await.unwrap();
    let fin_receiver_future_round = context
        .validate_proposal(
            ProposalInit { round: 2, ..Default::default() },
            TIMEOUT,
            content_receiver,
        )
        .await;
    content_sender.close_channel();
    // Even with sending fin and closing the channel.
    assert!(fin_receiver_future_round.now_or_never().is_none());
}

#[tokio::test]
async fn interrupt_active_proposal() {
    let mut batcher = MockBatcherClient::new();
    setup_batcher_for_validate(&mut batcher, BlockNumber(0)).await;
    let (default_deps, _network) = default_context_dependencies();
    let context_deps = SequencerConsensusContextDeps { batcher: Arc::new(batcher), ..default_deps };
    let mut context = setup_with_custom_mocks(context_deps);
    // Initialize the context for a specific height, starting with round 0.
    context.set_height_and_round(BlockNumber(0), 0).await;

    // Keep the sender open, as closing it or sending Fin would cause the validate to complete
    // without needing interrupt.
    let (mut _content_sender_0, content_receiver) =
        mpsc::channel(context.config.proposal_buffer_size);
    let fin_receiver_0 =
        context.validate_proposal(ProposalInit::default(), TIMEOUT, content_receiver).await;

    let (mut content_sender_1, content_receiver) =
        mpsc::channel(context.config.proposal_buffer_size);
    content_sender_1.send(ProposalPart::BlockInfo(block_info(BlockNumber(0)))).await.unwrap();
    content_sender_1
        .send(ProposalPart::Transactions(TransactionBatch { transactions: TX_BATCH.to_vec() }))
        .await
        .unwrap();
    content_sender_1
        .send(ProposalPart::Fin(ProposalFin {
            proposal_commitment: BlockHash(STATE_DIFF_COMMITMENT.0.0),
        }))
        .await
        .unwrap();
    let fin_receiver_1 = context
        .validate_proposal(
            ProposalInit { round: 1, ..Default::default() },
            TIMEOUT,
            content_receiver,
        )
        .await;
    // Move the context to the next round.
    context.set_height_and_round(BlockNumber(0), 1).await;

    // Interrupt active proposal.
    assert!(fin_receiver_0.await.is_err());
    assert_eq!(fin_receiver_1.await.unwrap().0, STATE_DIFF_COMMITMENT.0.0);
}

#[tokio::test]
async fn build_proposal() {
    let before: u64 =
        chrono::Utc::now().timestamp().try_into().expect("Timestamp conversion failed");
    let mut batcher = MockBatcherClient::new();
    setup_batcher_for_build(&mut batcher, BlockNumber(0)).await;
    let (default_deps, mut network) = default_context_dependencies();
    let context_deps = SequencerConsensusContextDeps { batcher: Arc::new(batcher), ..default_deps };
    let mut context = setup_with_custom_mocks(context_deps);
    let fin_receiver = context.build_proposal(ProposalInit::default(), TIMEOUT).await;
    // Test proposal parts.
    let (_, mut receiver) = network.outbound_proposal_receiver.next().await.unwrap();
    assert_eq!(receiver.next().await.unwrap(), ProposalPart::Init(ProposalInit::default()));
    let block_info = receiver.next().await.unwrap();
    let after: u64 =
        chrono::Utc::now().timestamp().try_into().expect("Timestamp conversion failed");
    let ProposalPart::BlockInfo(info) = block_info else {
        panic!("Expected ProposalPart::BlockInfo");
    };
    assert!(info.timestamp >= before && info.timestamp <= after);
    assert_eq!(info.eth_to_fri_rate, ETH_TO_FRI_RATE);
    assert_eq!(
        receiver.next().await.unwrap(),
        ProposalPart::Transactions(TransactionBatch { transactions: TX_BATCH.to_vec() })
    );
    assert_eq!(
        receiver.next().await.unwrap(),
        ProposalPart::Fin(ProposalFin {
            proposal_commitment: BlockHash(STATE_DIFF_COMMITMENT.0.0),
        })
    );
    assert!(receiver.next().await.is_none());
    assert_eq!(fin_receiver.await.unwrap().0, STATE_DIFF_COMMITMENT.0.0);
}

#[tokio::test]
async fn build_proposal_cende_failure() {
    let mut mock_cende_context = MockCendeContext::new();
    mock_cende_context
        .expect_write_prev_height_blob()
        .times(1)
        .return_once(|_height| tokio::spawn(ready(false)));
    let mut batcher = MockBatcherClient::new();
    setup_batcher_for_build(&mut batcher, BlockNumber(0)).await;
    let (default_deps, _network) = default_context_dependencies();
    let context_deps = SequencerConsensusContextDeps {
        batcher: Arc::new(batcher),
        cende_ambassador: Arc::new(mock_cende_context),
        ..default_deps
    };
    let mut context = setup_with_custom_mocks(context_deps);

    let fin_receiver = context.build_proposal(ProposalInit::default(), TIMEOUT).await;
    assert_eq!(fin_receiver.await, Err(Canceled));
}

#[tokio::test]
async fn build_proposal_cende_incomplete() {
    let mut mock_cende_context = MockCendeContext::new();
    mock_cende_context
        .expect_write_prev_height_blob()
        .times(1)
        .return_once(|_height| tokio::spawn(pending()));
    let mut batcher = MockBatcherClient::new();
    setup_batcher_for_build(&mut batcher, BlockNumber(0)).await;
    let (default_deps, _network) = default_context_dependencies();
    let context_deps = SequencerConsensusContextDeps {
        batcher: Arc::new(batcher),
        cende_ambassador: Arc::new(mock_cende_context),
        ..default_deps
    };
    let mut context = setup_with_custom_mocks(context_deps);

    let fin_receiver = context.build_proposal(ProposalInit::default(), TIMEOUT).await;
    assert_eq!(fin_receiver.await, Err(Canceled));
}

#[rstest]
#[case::proposer(true)]
#[case::validator(false)]
#[tokio::test]
async fn batcher_not_ready(#[case] proposer: bool) {
    let mut batcher = MockBatcherClient::new();
    batcher.expect_start_height().times(1).return_const(Ok(()));
    if proposer {
        batcher
            .expect_propose_block()
            .times(1)
            .return_const(Err(BatcherClientError::BatcherError(BatcherError::NotReady)));
    } else {
        batcher
            .expect_validate_block()
            .times(1)
            .return_const(Err(BatcherClientError::BatcherError(BatcherError::NotReady)));
    }
    let (default_deps, _network) = default_context_dependencies();
    let context_deps = SequencerConsensusContextDeps { batcher: Arc::new(batcher), ..default_deps };
    let mut context = setup_with_custom_mocks(context_deps);
    context.set_height_and_round(BlockNumber::default(), Round::default()).await;

    if proposer {
        let fin_receiver = context.build_proposal(ProposalInit::default(), TIMEOUT).await;
        assert_eq!(fin_receiver.await, Err(Canceled));
    } else {
        let (mut content_sender, content_receiver) =
            mpsc::channel(context.config.proposal_buffer_size);
        content_sender.send(ProposalPart::BlockInfo(block_info(BlockNumber(0)))).await.unwrap();

        let fin_receiver =
            context.validate_proposal(ProposalInit::default(), TIMEOUT, content_receiver).await;
        assert_eq!(fin_receiver.await, Err(Canceled));
    }
}

#[tokio::test]
async fn propose_then_repropose() {
    let mut batcher = MockBatcherClient::new();
    setup_batcher_for_build(&mut batcher, BlockNumber(0)).await;
    let (default_deps, mut network) = default_context_dependencies();
    let context_deps = SequencerConsensusContextDeps { batcher: Arc::new(batcher), ..default_deps };
    let mut context = setup_with_custom_mocks(context_deps);
    // Build proposal.
    let fin_receiver = context.build_proposal(ProposalInit::default(), TIMEOUT).await;
    let (_, mut receiver) = network.outbound_proposal_receiver.next().await.unwrap();
    // Receive the proposal parts.
    let _init = receiver.next().await.unwrap();
    let block_info = receiver.next().await.unwrap();
    let txs = receiver.next().await.unwrap();
    let fin = receiver.next().await.unwrap();
    assert_eq!(fin_receiver.await.unwrap().0, STATE_DIFF_COMMITMENT.0.0);

    // Re-propose.
    context
        .repropose(
            BlockHash(STATE_DIFF_COMMITMENT.0.0),
            ProposalInit { round: 1, ..Default::default() },
        )
        .await;
    // Re-propose sends the same proposal.
    let (_, mut receiver) = network.outbound_proposal_receiver.next().await.unwrap();
    let _init = receiver.next().await.unwrap();
    assert_eq!(receiver.next().await.unwrap(), block_info);
    assert_eq!(receiver.next().await.unwrap(), txs);
    assert_eq!(receiver.next().await.unwrap(), fin);
    assert!(receiver.next().await.is_none());
}

#[tokio::test]
async fn eth_to_fri_rate_out_of_range() {
    let mut batcher = MockBatcherClient::new();

    batcher
        .expect_start_height()
        .times(1)
        .withf(|input| input.height == BlockNumber(0))
        .return_const(Ok(()));
    let (default_deps, _network) = default_context_dependencies();
    let context_deps = SequencerConsensusContextDeps { batcher: Arc::new(batcher), ..default_deps };
    let mut context = setup_with_custom_mocks(context_deps);
    context.set_height_and_round(BlockNumber(0), 0).await;
    let (mut content_sender, content_receiver) = mpsc::channel(context.config.proposal_buffer_size);
    // Send a block info with an eth_to_fri_rate that is outside the margin of error.
    let mut block_info = block_info(BlockNumber(0));
    block_info.eth_to_fri_rate *= 2;
    content_sender.send(ProposalPart::BlockInfo(block_info).clone()).await.unwrap();
    // Use a large enough timeout to ensure fin_receiver was canceled due to invalid block_info,
    // not due to a timeout.
    let fin_receiver =
        context.validate_proposal(ProposalInit::default(), TIMEOUT * 100, content_receiver).await;
    assert_eq!(fin_receiver.await, Err(Canceled));
    // TODO(guyn): How to check that the rejection is due to the eth_to_fri_rate?
}

#[rstest]
#[case::maximum(true)]
#[case::minimum(false)]
#[tokio::test]
async fn gas_price_limits(#[case] maximum: bool) {
    let context_config = ContextConfig::default();
    let min_gas_price = context_config.min_l1_gas_price_wei;
    let min_data_price = context_config.min_l1_data_gas_price_wei;
    let max_gas_price = context_config.max_l1_gas_price_wei;
    let max_data_price = context_config.max_l1_data_gas_price_wei;

    let price = if maximum {
        // Take the higher maximum price and go much higher than that.
        // If we don't go much higher, the l1_data_gas_price_multiplier will
        // lower the data gas price below the clamp limit.
        std::cmp::max(max_gas_price, max_data_price) * 100
    } else {
        0
    };
    let mut l1_gas_price_provider = MockL1GasPriceProviderClient::new();
    l1_gas_price_provider.expect_get_price_info().returning(move |_| {
        Ok(PriceInfo { base_fee_per_gas: GasPrice(price), blob_fee: GasPrice(price) })
    });
    let l1_gas_price_provider = Arc::new(l1_gas_price_provider);
    let mut batcher = MockBatcherClient::new();
    setup_batcher_for_validate(&mut batcher, BlockNumber(0)).await;

    let (default_deps, _network) = default_context_dependencies();
    let context_deps = SequencerConsensusContextDeps {
        batcher: Arc::new(batcher),
        l1_gas_price_provider,
        ..default_deps
    };
    let mut context = setup_with_custom_mocks(context_deps);

    context.set_height_and_round(BlockNumber(0), 0).await;
    let (mut content_sender, content_receiver) = mpsc::channel(context.config.proposal_buffer_size);

    let mut block_info = block_info(BlockNumber(0));

    if maximum {
        // Set the gas price to the maximum value.
        block_info.l1_gas_price_wei = GasPrice(max_gas_price);
        block_info.l1_data_gas_price_wei = GasPrice(max_data_price);
    } else {
        // Set the gas price to the minimum value.
        block_info.l1_gas_price_wei = GasPrice(min_gas_price);
        block_info.l1_data_gas_price_wei = GasPrice(min_data_price);
    }

    // Send the block info, some transactions and then fin.
    content_sender.send(ProposalPart::BlockInfo(block_info).clone()).await.unwrap();
    content_sender
        .send(ProposalPart::Transactions(TransactionBatch { transactions: TX_BATCH.to_vec() }))
        .await
        .unwrap();
    content_sender
        .send(ProposalPart::Fin(ProposalFin {
            proposal_commitment: BlockHash(STATE_DIFF_COMMITMENT.0.0),
        }))
        .await
        .unwrap();

    // Even though we used the minimum/maximum gas price, not the values we gave the provider,
    // the proposal should be still be valid due to the clamping of limit prices.
    let fin_receiver =
        context.validate_proposal(ProposalInit::default(), TIMEOUT, content_receiver).await;
    assert_eq!(fin_receiver.await, Ok(BlockHash(STATE_DIFF_COMMITMENT.0.0)));
}

#[tokio::test]
async fn decision_reached_sends_correct_values() {
    let recorder = PrometheusBuilder::new().build_recorder();
    let _recorder_guard = metrics::set_default_local_recorder(&recorder);
    // We need to create a valid proposal to call decision_reached on.
    //
    // 1. Build proposal setup starts.
    let mut batcher = MockBatcherClient::new();
    setup_batcher_for_build(&mut batcher, BlockNumber(0)).await;

    const BLOCK_TIME_STAMP_SECONDS: u64 = 123456;
    let mut clock = MockClock::new();
    clock.expect_now_as_timestamp().return_const(BLOCK_TIME_STAMP_SECONDS);
    clock
        .expect_now()
        .return_const(Utc.timestamp_opt(BLOCK_TIME_STAMP_SECONDS.try_into().unwrap(), 0).unwrap());

    // 2. Decision reached setup starts.
    batcher
        .expect_decision_reached()
        .times(1)
        .return_once(move |_| Ok(DecisionReachedResponse::default()));

    // Mock the sync client and validate that add_new_block receives the expected block_info.
    let mut mock_sync_client = MockStateSyncClient::new();

    // This is the actual part of the test that checks the values are correct.
    // TODO(guy.f): Add expectations and validations for all the other values being written.
    mock_sync_client.expect_add_new_block().times(1).return_once(|block_info| {
        assert_eq!(block_info.block_header_without_hash.timestamp.0, BLOCK_TIME_STAMP_SECONDS);
        Ok(())
    });

    let mut cende_ammbassador = success_cende_ammbassador();
    cende_ammbassador
        .expect_prepare_blob_for_next_height()
        // TODO(guy.f): Verify the values sent here are correct.
        .return_once(|_height| Ok(()));

    let (default_deps, _network_dependencies) = default_context_dependencies();
    let context_deps = SequencerConsensusContextDeps {
        batcher: Arc::new(batcher),
        cende_ambassador: Arc::new(cende_ammbassador),
        state_sync_client: Arc::new(mock_sync_client),
        clock: Arc::new(clock),
        ..default_deps
    };

    let mut context = setup_with_custom_mocks(context_deps);

    // This sets up the required state for the test, prior to running the code being tested.
    let _fin = context.build_proposal(ProposalInit::default(), TIMEOUT).await.await;
    // At this point we should have a valid proposal in the context which contains the timestamp.

    let vote = Vote {
        // Currently this is the only field used by decision_reached.
        height: 0,
        ..Default::default()
    };

    context.decision_reached(BlockHash(STATE_DIFF_COMMITMENT.0.0), vec![vote]).await.unwrap();

    let metrics = recorder.handle().render();
    CONSENSUS_L2_GAS_PRICE
        .assert_eq(&metrics, VersionedConstants::latest_constants().min_gas_price.0);
}

#[rstest]
#[case::l1_price_oracle_failure(true)]
#[case::eth_to_strk_rate_oracle_failure(false)]
#[tokio::test]
async fn oracle_fails_on_startup(#[case] l1_oracle_failure: bool) {
    let mut batcher = MockBatcherClient::new();
    setup_batcher_for_build(&mut batcher, BlockNumber(0)).await;

    let (l1_prices_oracle_client, eth_to_strk_oracle_client) = {
        if l1_oracle_failure {
            let mut l1_prices_oracle_client = MockL1GasPriceProviderClient::new();
            l1_prices_oracle_client.expect_get_price_info().times(1).return_const(Err(
                L1GasPriceClientError::L1GasPriceProviderError(
                    // random error, these parameters don't mean anything
                    L1GasPriceProviderError::UnexpectedHeightError { expected: 0, found: 1 },
                ),
            ));
            let eth_to_strk_orcale_client = dummy_eth_to_strk_oracle_client();
            (l1_prices_oracle_client, eth_to_strk_orcale_client)
        } else {
            let mut eth_to_strk_oracle_client = MockEthToStrkOracleClientTrait::new();
            eth_to_strk_oracle_client
                .expect_eth_to_fri_rate()
                .times(1)
                .return_once(|_| Err(EthToStrkOracleClientError::MissingFieldError("")));
            let l1_prices_oracle_client = dummy_gas_price_provider();
            (l1_prices_oracle_client, eth_to_strk_oracle_client)
        }
    };

    let (default_dependencies, mut network) = default_context_dependencies();

    let context_dependencies = SequencerConsensusContextDeps {
        batcher: Arc::new(batcher),
        eth_to_strk_oracle_client: Arc::new(eth_to_strk_oracle_client),
        l1_gas_price_provider: Arc::new(l1_prices_oracle_client),
        ..default_dependencies
    };

    let mut context = setup_with_custom_mocks(context_dependencies);

    let init = ProposalInit::default();

    let fin_receiver = context.build_proposal(init, TIMEOUT).await;

    let (_, mut receiver) = network.outbound_proposal_receiver.next().await.unwrap();

    assert_eq!(receiver.next().await.unwrap(), ProposalPart::Init(ProposalInit::default()));
    let block_info = receiver.next().await.unwrap();
    let ProposalPart::BlockInfo(info) = block_info else {
        panic!("Expected ProposalPart::BlockInfo");
    };

    let default_context_config = ContextConfig::default();
    assert_eq!(info.eth_to_fri_rate, DEFAULT_ETH_TO_FRI_RATE);
    // Despite the l1_gas_price_provider being set up not to fail, we still expect the default
    // values because eth_to_strk_rate_oracle_client failed.
    assert_eq!(info.l1_gas_price_wei.0, default_context_config.min_l1_gas_price_wei);
    assert_eq!(info.l1_data_gas_price_wei.0, default_context_config.min_l1_data_gas_price_wei);

    assert_eq!(
        receiver.next().await.unwrap(),
        ProposalPart::Transactions(TransactionBatch { transactions: TX_BATCH.to_vec() })
    );
    assert_eq!(
        receiver.next().await.unwrap(),
        ProposalPart::Fin(ProposalFin {
            proposal_commitment: BlockHash(STATE_DIFF_COMMITMENT.0.0),
        })
    );
    assert!(receiver.next().await.is_none());
    assert_eq!(fin_receiver.await.unwrap().0, STATE_DIFF_COMMITMENT.0.0);
}

#[rstest]
#[case::l1_price_oracle_failure(true)]
#[case::eth_to_strk_rate_oracle_failure(false)]
#[tokio::test]
async fn oracle_fails_on_second_block(#[case] l1_oracle_failure: bool) {
    // Validate block number 0, call decision_reached to save the previous block info (block 0), and
    // attempt to build_proposal on block number 1.
    let mut batcher = MockBatcherClient::new();
    setup_batcher_for_validate(&mut batcher, BlockNumber(0)).await;
    setup_batcher_for_build(&mut batcher, BlockNumber(1)).await;

    // set up batcher decision_reached
    batcher.expect_decision_reached().times(1).return_once(|_| {
        Ok(DecisionReachedResponse {
            state_diff: ThinStateDiff::default(),
            l2_gas_used: GasAmount::default(),
            central_objects: CentralObjects::default(),
        })
    });

    // required for decision reached flow
    let mut state_sync_client = MockStateSyncClient::new();
    state_sync_client.expect_add_new_block().times(1).return_once(|_| Ok(()));
    let mut cende_ambassador = success_cende_ammbassador();
    cende_ambassador.expect_prepare_blob_for_next_height().times(1).return_once(|_| Ok(()));

    // set the oracle to succeed on first block and fail on second
    let (l1_prices_oracle_client, eth_to_strk_oracle_client) = {
        if l1_oracle_failure {
            let mut l1_prices_oracle_client = MockL1GasPriceProviderClient::new();
            l1_prices_oracle_client.expect_get_price_info().times(1).return_const(Ok(PriceInfo {
                base_fee_per_gas: GasPrice(TEMP_ETH_GAS_FEE_IN_WEI),
                blob_fee: GasPrice(TEMP_ETH_BLOB_GAS_FEE_IN_WEI),
            }));
            l1_prices_oracle_client.expect_get_price_info().times(1).return_const(Err(
                L1GasPriceClientError::L1GasPriceProviderError(
                    // random error, these parameters don't mean anything
                    L1GasPriceProviderError::UnexpectedHeightError { expected: 0, found: 1 },
                ),
            ));
            let eth_to_strk_orcale_client = dummy_eth_to_strk_oracle_client();
            (l1_prices_oracle_client, eth_to_strk_orcale_client)
        } else {
            let mut eth_to_strk_oracle_client = MockEthToStrkOracleClientTrait::new();
            eth_to_strk_oracle_client
                .expect_eth_to_fri_rate()
                .times(1)
                .return_once(|_| Ok(ETH_TO_FRI_RATE));
            eth_to_strk_oracle_client
                .expect_eth_to_fri_rate()
                .times(1)
                .return_once(|_| Err(EthToStrkOracleClientError::MissingFieldError("")));
            let l1_prices_oracle_client = dummy_gas_price_provider();
            (l1_prices_oracle_client, eth_to_strk_oracle_client)
        }
    };

    let (default_dependencies, mut network) = default_context_dependencies();

    let context_dependencies = SequencerConsensusContextDeps {
        batcher: Arc::new(batcher),
        cende_ambassador: Arc::new(cende_ambassador),
        eth_to_strk_oracle_client: Arc::new(eth_to_strk_oracle_client),
        l1_gas_price_provider: Arc::new(l1_prices_oracle_client),
        state_sync_client: Arc::new(state_sync_client),
        ..default_dependencies
    };

    let mut context = setup_with_custom_mocks(context_dependencies);

    // Validate block number 0.

    // Initialize the context for a specific height, starting with round 0.
    context.set_height_and_round(BlockNumber(0), 0).await;

    let (mut content_sender, content_receiver) = mpsc::channel(context.config.proposal_buffer_size);
    content_sender.send(ProposalPart::BlockInfo(block_info(BlockNumber(0)))).await.unwrap();
    content_sender
        .send(ProposalPart::Transactions(TransactionBatch { transactions: TX_BATCH.to_vec() }))
        .await
        .unwrap();
    content_sender
        .send(ProposalPart::Fin(ProposalFin {
            proposal_commitment: BlockHash(STATE_DIFF_COMMITMENT.0.0),
        }))
        .await
        .unwrap();
    let fin_receiver =
        context.validate_proposal(ProposalInit::default(), TIMEOUT, content_receiver).await;
    content_sender.close_channel();
    let block_hash = fin_receiver.await.unwrap().0;
    assert_eq!(block_hash, STATE_DIFF_COMMITMENT.0.0);

    // Decision reached

    context
        .decision_reached(
            BlockHash(block_hash),
            vec![Vote { block_hash: Some(BlockHash(block_hash)), ..Default::default() }],
        )
        .await
        .unwrap();

    // Build proposal for block number 1.
    let init = ProposalInit { height: BlockNumber(1), ..Default::default() };

    let fin_receiver = context.build_proposal(init, TIMEOUT).await;

    let (_, mut receiver) = network.outbound_proposal_receiver.next().await.unwrap();

    assert_eq!(
        receiver.next().await.unwrap(),
        ProposalPart::Init(ProposalInit { height: BlockNumber(1), ..Default::default() })
    );
    let info = receiver.next().await.unwrap();
    let ProposalPart::BlockInfo(info) = info else {
        panic!("Expected ProposalPart::BlockInfo");
    };

    let previous_block_info = block_info(BlockNumber(0));
    // The l1_data_gas_price the proposer sends contains a discount we need to adjust for here.
    let ContextConfig { l1_data_gas_price_multiplier_ppt: default_data_gas_multiplier, .. } =
        ContextConfig::default();
    let expected_l1_data_gas_price =
        GasPrice(previous_block_info.l1_data_gas_price_wei.0 * default_data_gas_multiplier / 1000);

    assert_eq!(info.eth_to_fri_rate, previous_block_info.eth_to_fri_rate);
    assert_eq!(info.l1_gas_price_wei, previous_block_info.l1_gas_price_wei);
    assert_eq!(info.l1_data_gas_price_wei, expected_l1_data_gas_price);

    assert_eq!(
        receiver.next().await.unwrap(),
        ProposalPart::Transactions(TransactionBatch { transactions: TX_BATCH.to_vec() })
    );
    assert_eq!(
        receiver.next().await.unwrap(),
        ProposalPart::Fin(ProposalFin {
            proposal_commitment: BlockHash(STATE_DIFF_COMMITMENT.0.0),
        })
    );
    assert!(receiver.next().await.is_none());
    assert_eq!(fin_receiver.await.unwrap().0, STATE_DIFF_COMMITMENT.0.0);
}

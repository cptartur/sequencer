use assert_matches::assert_matches;
use blockifier_test_utils::cairo_versions::{CairoVersion, RunnableCairo1};
use blockifier_test_utils::contracts::FeatureContract;
use rstest::rstest;
use starknet_api::executable_transaction::AccountTransaction as Transaction;
use starknet_api::transaction::fields::ValidResourceBounds;
use starknet_api::transaction::TransactionVersion;

use crate::blockifier::stateful_validator::StatefulValidator;
use crate::context::BlockContext;
use crate::test_utils::initial_test_state::{fund_account, test_state};
use crate::test_utils::BALANCE;
use crate::transaction::test_utils::{
    block_context,
    create_account_tx_for_validate_test_nonce_0,
    default_all_resource_bounds,
    default_l1_resource_bounds,
    FaultyAccountTxCreatorArgs,
    INVALID,
    VALID,
};
use crate::transaction::transaction_types::TransactionType;

#[rstest]
#[case::validate_version_1(TransactionType::InvokeFunction, false, TransactionVersion::ONE)]
#[case::validate_version_3(TransactionType::InvokeFunction, false, TransactionVersion::THREE)]
#[case::validate_declare_version_1(TransactionType::Declare, false, TransactionVersion::ONE)]
#[case::validate_declare_version_2(TransactionType::Declare, false, TransactionVersion::TWO)]
#[case::validate_declare_version_3(TransactionType::Declare, false, TransactionVersion::THREE)]
#[case::validate_deploy_version_1(TransactionType::DeployAccount, false, TransactionVersion::ONE)]
#[case::validate_deploy_version_3(TransactionType::DeployAccount, false, TransactionVersion::THREE)]
#[case::constructor_version_1(TransactionType::DeployAccount, true, TransactionVersion::ONE)]
#[case::constructor_version_3(TransactionType::DeployAccount, true, TransactionVersion::THREE)]
fn test_tx_validator(
    #[case] tx_type: TransactionType,
    #[case] validate_constructor: bool,
    #[case] tx_version: TransactionVersion,
    block_context: BlockContext,
    #[values(default_l1_resource_bounds(), default_all_resource_bounds())]
    resource_bounds: ValidResourceBounds,
    #[values(CairoVersion::Cairo0, CairoVersion::Cairo1(RunnableCairo1::Casm))]
    cairo_version: CairoVersion,
) {
    let chain_info = &block_context.chain_info;

    // TODO(Arni, 1/5/2024): Add test for insufficient balance.
    let account_balance = BALANCE;
    let faulty_account = FeatureContract::FaultyAccount(cairo_version);
    let sender_address = faulty_account.get_instance_address(0);
    let class_hash = faulty_account.get_class_hash();

    let mut state = test_state(chain_info, account_balance, &[(faulty_account, 1)]);

    let tx_args = FaultyAccountTxCreatorArgs {
        tx_type,
        tx_version,
        sender_address,
        class_hash,
        validate_constructor,
        // TODO(Arni, 1/5/2024): Add test for insufficient maximal resources.
        max_fee: BALANCE,
        resource_bounds,
        ..Default::default()
    };

    // Positive flow.
    let account_tx = create_account_tx_for_validate_test_nonce_0(FaultyAccountTxCreatorArgs {
        scenario: VALID,
        ..tx_args
    });
    if let Transaction::DeployAccount(deploy_tx) = &account_tx.tx {
        fund_account(chain_info, deploy_tx.contract_address(), BALANCE, &mut state.state);
    }

    // Test the stateful validator.
    let mut stateful_validator = StatefulValidator::create(state, block_context);
    let skip_validate = false;
    let result = stateful_validator.perform_validations(account_tx, skip_validate);
    assert!(result.is_ok(), "Validation failed: {:?}", result.unwrap_err());
}

#[rstest]
fn test_tx_validator_skip_validate(
    #[values(default_l1_resource_bounds(), default_all_resource_bounds())]
    resource_bounds: ValidResourceBounds,
) {
    let block_context = BlockContext::create_for_testing();
    let faulty_account = FeatureContract::FaultyAccount(CairoVersion::Cairo1(RunnableCairo1::Casm));
    let state = test_state(&block_context.chain_info, BALANCE, &[(faulty_account, 1)]);

    // Create a transaction that does not pass validations.
    let tx = create_account_tx_for_validate_test_nonce_0(FaultyAccountTxCreatorArgs {
        scenario: INVALID,
        tx_type: TransactionType::InvokeFunction,
        tx_version: TransactionVersion::THREE,
        sender_address: faulty_account.get_instance_address(0),
        class_hash: faulty_account.get_class_hash(),
        resource_bounds,
        ..Default::default()
    });

    let mut stateful_validator = StatefulValidator::create(state, block_context);
    // The transaction validations should be skipped and the function should return Ok.
    let result = stateful_validator.perform_validations(tx, true);
    assert_matches!(result, Ok(()));
}

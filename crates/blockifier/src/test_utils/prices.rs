use std::sync::Arc;

use cached::proc_macro::cached;
use cairo_vm::vm::runners::cairo_runner::ExecutionResources;
use starknet_api::abi::abi_utils::{get_fee_token_var_address, selector_from_name};
use starknet_api::block::FeeType;
use starknet_api::core::ContractAddress;
use starknet_api::execution_resources::GasAmount;
use starknet_api::test_utils::invoke::InvokeTxArgs;
use starknet_api::transaction::constants;
use starknet_api::{calldata, felt};

use crate::context::BlockContext;
use crate::execution::common_hints::ExecutionMode;
use crate::execution::entry_point::{
    CallEntryPoint,
    EntryPointExecutionContext,
    SierraGasRevertTracker,
};
use crate::state::state_api::State;
use crate::test_utils::initial_test_state::test_state;
use crate::test_utils::BALANCE;
use crate::transaction::test_utils::invoke_tx_with_default_flags;

/// Enum for all resource costs.
pub enum Prices {
    // Fee transfer cost depends non-trivially on the account contract address (address is part of
    // a hash preimage, the value of which effects the flow of `normalize_address`).
    FeeTransfer(ContractAddress, FeeType),
}

impl From<Prices> for ExecutionResources {
    fn from(value: Prices) -> Self {
        match value {
            Prices::FeeTransfer(contract_address, fee_type) => {
                fee_transfer_resources(contract_address, fee_type)
            }
        }
    }
}

/// Returns the expected VM resource consumption for a fee transfer call from the given address.
#[cached]
fn fee_transfer_resources(
    account_contract_address: ContractAddress,
    fee_type: FeeType,
) -> ExecutionResources {
    let block_context = &BlockContext::create_for_account_testing();
    let chain_info = &block_context.chain_info;
    let state = &mut test_state(chain_info, BALANCE, &[]);
    let token_address = chain_info.fee_token_address(&fee_type);

    // Fund the account so we don't hit an error.
    state
        .set_storage_at(
            token_address,
            get_fee_token_var_address(account_contract_address),
            felt!(BALANCE.0),
        )
        .unwrap();

    // Execute a fee transfer call and return the VM resources used.
    let fee_transfer_call = CallEntryPoint {
        entry_point_selector: selector_from_name(constants::TRANSFER_ENTRY_POINT_NAME),
        calldata: calldata![
            *block_context.block_info.sequencer_address.0.key(), // Recipient.
            felt!(7_u8),                                         // LSB of Amount.
            felt!(0_u8)                                          // MSB of Amount.
        ],
        storage_address: token_address,
        caller_address: account_contract_address,
        ..Default::default()
    };
    let mut remaining_gas = fee_transfer_call.initial_gas;
    fee_transfer_call
        .execute(
            state,
            &mut EntryPointExecutionContext::new(
                Arc::new(
                    block_context
                        .to_tx_context(&invoke_tx_with_default_flags(InvokeTxArgs::default())),
                ),
                ExecutionMode::Execute,
                false,
                // No need to limit gas in fee transfer.
                SierraGasRevertTracker::new(GasAmount::MAX),
            ),
            &mut remaining_gas,
        )
        .unwrap()
        .resources
}

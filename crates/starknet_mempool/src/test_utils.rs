use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use pretty_assertions::assert_eq;
use starknet_api::rpc_transaction::InternalRpcTransaction;
use starknet_api::transaction::TransactionHash;
use starknet_api::{contract_address, nonce};
use starknet_mempool_types::errors::MempoolError;
use starknet_mempool_types::mempool_types::{AddTransactionArgs, CommitBlockArgs};

use crate::mempool::Mempool;
use crate::utils::Clock;

/// Creates an executable invoke transaction with the given field subset (the rest receive default
/// values).
#[macro_export]
macro_rules! tx {
    (
        tx_hash: $tx_hash:expr,
        address: $address:expr,
        tx_nonce: $tx_nonce:expr,
        tip: $tip:expr,
        max_l2_gas_price: $max_l2_gas_price:expr
    ) => {{
            use starknet_api::block::GasPrice;
            use starknet_api::{invoke_tx_args, tx_hash};
            use starknet_api::test_utils::invoke::internal_invoke_tx;
            use starknet_api::transaction::fields::{
                AllResourceBounds,
                ResourceBounds,
                Tip,
                ValidResourceBounds,
            };

            let resource_bounds = ValidResourceBounds::AllResources(AllResourceBounds {
                l2_gas: ResourceBounds {
                    max_price_per_unit: GasPrice($max_l2_gas_price),
                    ..Default::default()
                },
                ..Default::default()
            });

            internal_invoke_tx(invoke_tx_args!{
                tx_hash: tx_hash!($tx_hash),
                sender_address: contract_address!($address),
                nonce: nonce!($tx_nonce),
                tip: Tip($tip),
                resource_bounds,
            })
    }};
    (tx_hash: $tx_hash:expr, address: $address:expr, tx_nonce: $tx_nonce:expr, tip: $tip:expr) => {{
        use mempool_test_utils::starknet_api_test_utils::VALID_L2_GAS_MAX_PRICE_PER_UNIT;
        tx!(
            tx_hash: $tx_hash,
            address: $address,
            tx_nonce: $tx_nonce,
            tip: $tip,
            max_l2_gas_price: VALID_L2_GAS_MAX_PRICE_PER_UNIT
        )
    }};
    (tx_hash: $tx_hash:expr, address: $address:expr, tx_nonce: $tx_nonce:expr) => {
        tx!(tx_hash: $tx_hash, address: $address, tx_nonce: $tx_nonce, tip: 0)
    };
    (tx_hash: $tx_hash:expr, address: $address:expr, tip: $tip:expr) => {
        tx!(tx_hash: $tx_hash, address: $address, tx_nonce: 0, tip: $tip)
    };
    (tx_hash: $tx_hash:expr, address: $address:expr, max_l2_gas_price: $max_l2_gas_price:expr) => {
        tx!(
            tx_hash: $tx_hash,
            address: $address,
            tx_nonce: 0,
            tip: 0,
            max_l2_gas_price: $max_l2_gas_price
        )
    };
    (tx_hash: $tx_hash:expr, tip: $tip:expr, max_l2_gas_price: $max_l2_gas_price:expr) => {
        tx!(
            tx_hash: $tx_hash,
            address: "0x0",
            tx_nonce: 0,
            tip: $tip,
            max_l2_gas_price: $max_l2_gas_price
        )
    };
    (tip: $tip:expr, max_l2_gas_price: $max_l2_gas_price:expr) => {
        tx!(tx_hash: 0, address: "0x0", tx_nonce: 0, tip: $tip, max_l2_gas_price: $max_l2_gas_price)
    };
    () => {
        tx!(tx_hash: 0, address: "0x0", tx_nonce: 0)
    };
}

/// Creates an input for `add_tx` with the given field subset (the rest receive default values).
#[macro_export]
macro_rules! add_tx_input {
    (
        tx_hash: $tx_hash:expr,
        address: $address:expr,
        tx_nonce: $tx_nonce:expr,
        account_nonce: $account_nonce:expr,
        tip: $tip:expr,
        max_l2_gas_price: $max_l2_gas_price:expr
    ) => {{
        use starknet_api::{contract_address, nonce};
        use starknet_mempool_types::mempool_types::{AccountState, AddTransactionArgs};

        let tx = $crate::tx!(
            tx_hash: $tx_hash,
            address: $address,
            tx_nonce: $tx_nonce,
            tip: $tip,
            max_l2_gas_price: $max_l2_gas_price
        );
        let address = contract_address!($address);
        let account_nonce = nonce!($account_nonce);
        let account_state = AccountState { address, nonce: account_nonce };

        AddTransactionArgs { tx, account_state }
    }};
    (
        tx_hash: $tx_hash:expr,
        address: $address:expr,
        tx_nonce: $tx_nonce:expr,
        account_nonce: $account_nonce:expr,
        tip: $tip:expr
    ) => {{
        use mempool_test_utils::starknet_api_test_utils::VALID_L2_GAS_MAX_PRICE_PER_UNIT;
        add_tx_input!(
            tx_hash: $tx_hash,
            address: $address,
            tx_nonce: $tx_nonce,
            account_nonce: $account_nonce,
            tip: $tip,
            max_l2_gas_price: VALID_L2_GAS_MAX_PRICE_PER_UNIT
        )
    }};
    (
        tx_hash: $tx_hash:expr,
        address: $address:expr,
        tx_nonce: $tx_nonce:expr,
        tip: $tip:expr,
        max_l2_gas_price: $max_l2_gas_price:expr
    ) => {
        add_tx_input!(
            tx_hash: $tx_hash,
            address: $address,
            tx_nonce: $tx_nonce,
            account_nonce: 0,
            tip: $tip,
            max_l2_gas_price: $max_l2_gas_price
        )
    };
    (tx_hash: $tx_hash:expr, address: $address:expr, tip: $tip:expr) => {
        add_tx_input!(
            tx_hash: $tx_hash,
            address: $address,
            tx_nonce: 0,
            account_nonce: 0,
            tip: $tip
        )
    };
    (
        tx_hash: $tx_hash:expr,
        address: $address:expr,
        tx_nonce: $tx_nonce:expr,
        account_nonce: $account_nonce:expr
    ) => {
        add_tx_input!(
            tx_hash: $tx_hash,
            address: $address,
            tx_nonce: $tx_nonce,
            account_nonce: $account_nonce,
            tip: 0
        )
    };
    (tx_hash: $tx_hash:expr, tx_nonce: $tx_nonce:expr, account_nonce: $account_nonce:expr) => {
        add_tx_input!(
            tx_hash: $tx_hash,
            address: "0x0",
            tx_nonce: $tx_nonce,
            account_nonce: $account_nonce
        )
    };
    (tx_hash: $tx_hash:expr, tx_nonce: $tx_nonce:expr) => {
        add_tx_input!(tx_hash: $tx_hash, tx_nonce: $tx_nonce, account_nonce: 0)
    };
    (
        tx_hash: $tx_hash:expr,
        address: $address:expr,
        tip: $tip:expr,
        max_l2_gas_price: $max_l2_gas_price:expr
    ) => {
        add_tx_input!(
            tx_hash: $tx_hash,
            address: $address,
            tx_nonce: 0,
            account_nonce: 0,
            tip: $tip,
            max_l2_gas_price: $max_l2_gas_price
        )
    };
    (tx_hash: $tx_hash:expr, tip: $tip:expr, max_l2_gas_price: $max_l2_gas_price:expr) => {
        add_tx_input!(
            tx_hash: $tx_hash,
            address: "0x0",
            tip: $tip,
            max_l2_gas_price: $max_l2_gas_price
        )
    };
    (tip: $tip:expr, max_l2_gas_price: $max_l2_gas_price:expr) => {
        add_tx_input!(
            tx_hash: 0,
            address: "0x0",
            tx_nonce: 0,
            tip: $tip,
            max_l2_gas_price: $max_l2_gas_price
        )
    };
    (address: $address:expr) => {
        add_tx_input!(
            tx_hash: 0,
            address: $address,
            tip: 0
        )
    };
}

#[track_caller]
pub fn add_tx(mempool: &mut Mempool, input: &AddTransactionArgs) {
    assert_eq!(mempool.add_tx(input.clone()), Ok(()));
}

#[track_caller]
pub fn add_tx_expect_error(
    mempool: &mut Mempool,
    input: &AddTransactionArgs,
    expected_error: MempoolError,
) {
    assert_eq!(mempool.add_tx(input.clone()), Err(expected_error));
}

#[track_caller]
pub fn commit_block(
    mempool: &mut Mempool,
    nonces: impl IntoIterator<Item = (&'static str, u8)>,
    rejected_tx_hashes: impl IntoIterator<Item = TransactionHash>,
) {
    let nonces = HashMap::from_iter(
        nonces.into_iter().map(|(address, nonce)| (contract_address!(address), nonce!(nonce))),
    );
    let rejected_tx_hashes = rejected_tx_hashes.into_iter().collect();
    let args = CommitBlockArgs { address_to_nonce: nonces, rejected_tx_hashes };

    mempool.commit_block(args);
}

#[track_caller]
pub fn get_txs_and_assert_expected(
    mempool: &mut Mempool,
    n_txs: usize,
    expected_txs: &[InternalRpcTransaction],
) {
    let txs = mempool.get_txs(n_txs).unwrap();
    assert_eq!(txs, expected_txs);
}

pub struct FakeClock {
    pub now: Mutex<Instant>,
}

impl Default for FakeClock {
    fn default() -> Self {
        FakeClock { now: Mutex::new(Instant::now()) }
    }
}

impl FakeClock {
    pub fn advance(&self, duration: std::time::Duration) {
        *self.now.lock().unwrap() += duration;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        *self.now.lock().unwrap()
    }
}

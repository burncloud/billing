#![forbid(unsafe_code)]

mod contract;
mod wallet;

pub use contract::{run_wallet_contract, WalletContract};
pub use wallet::MemoryWallet;

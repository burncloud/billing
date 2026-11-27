#![forbid(unsafe_code)]

mod error;
mod wallet;

pub use error::PostgresError;
pub use wallet::PostgresWallet;

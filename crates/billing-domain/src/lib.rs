#![forbid(unsafe_code)]

mod invariant;
mod posting;
mod price_book;
mod pricing;

pub use invariant::{check_ledger, request_held_flat, CurrencyFunds, InvariantViolation};
pub use posting::{
    post_hold, post_release, post_settle, ExistingHold, HoldRequest, OpenHold, Posted,
    ReleaseRequest, SettleRequest, SpendSoFar, StorageFault, WalletSnapshot,
};
pub use price_book::{DuplicateModel, PriceBook, PriceLookupError};
pub use pricing::{
    ModelPrice, NanosPerMillionTokens, PriceRejection, PricingError, TOKENS_PER_PRICE_UNIT,
};

#[cfg(test)]
mod manifest {
    #[test]
    fn dependencies_stay_on_the_next_contracts() {
        let manifest = include_str!("../Cargo.toml");
        let bans = include_str!("../bans.toml");
        let mut names = Vec::new();
        let mut in_dependencies = false;
        for line in manifest.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_dependencies = trimmed == "[dependencies]";
                continue;
            }
            if !in_dependencies || trimmed.is_empty() {
                continue;
            }
            let Some((name, _)) = trimmed.split_once('=') else {
                continue;
            };
            names.push(name.trim());
        }
        assert_eq!(names, ["billing", "kernel"]);
        for name in ["sqlx", "tokio", "reqwest", "postgres", "hyper"] {
            assert!(!names.contains(&name), "{name}");
            let needle = format!("name = \"{name}\"");
            assert!(bans.contains(&needle), "{name}");
        }
    }
}

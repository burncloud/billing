use std::sync::{Arc, Barrier};
use std::thread;

use billing::errors::{BillingRejection, WalletError};
use billing::model::{Entry, KeyCap};
use billing_domain::{check_ledger, request_held_flat, CurrencyFunds, StorageFault};
use kernel::model::{AccountId, Currency, KeyId, Money, RequestId, UnixMillis};

/// 内存和以后的 Postgres 共用的钱包操作。`hold` 不创建余额行。
pub trait WalletContract: Sync {
    /// 已有该币种的行时，把金额加进可用余额和存入额。没有行时新建，冻结额为 0。
    ///
    /// # Errors
    ///
    /// 相加溢出时返回 `WalletError::Rejected(BillingRejection::Overflow)`。
    fn deposit(&self, account: &AccountId, amount: Money) -> Result<(), WalletError<StorageFault>>;

    /// # Errors
    ///
    /// 没有余额行时返回 `WalletError::Storage(StorageFault::MissingBalance)`，不插入行。
    /// 同一 `(account, request)` 已经冻结过时返回 `WalletError::Rejected(BillingRejection::DuplicateRequest)`。
    /// 超过 `cap` 时返回 `WalletError::Rejected(BillingRejection::LimitExceeded)`，不写分录。
    fn hold(
        &self,
        account: &AccountId,
        request: &RequestId,
        key: &KeyId,
        cap: Option<KeyCap>,
        amount: Money,
    ) -> Result<(), WalletError<StorageFault>>;

    /// # Errors
    ///
    /// 没有余额行时返回 `WalletError::Storage(StorageFault::MissingBalance)`。
    /// 没有冻结、已结算、已释放、币种不同或超过冻结额时，返回 `billing_domain::post_settle` 的对应错误。
    fn settle(
        &self,
        account: &AccountId,
        request: &RequestId,
        amount: Money,
    ) -> Result<(), WalletError<StorageFault>>;

    /// # Errors
    ///
    /// 没有余额行时返回 `WalletError::Storage(StorageFault::MissingBalance)`。
    /// 没有冻结、已释放或已结算时，返回 `billing_domain::post_release` 的对应错误。
    fn release(
        &self,
        account: &AccountId,
        request: &RequestId,
    ) -> Result<(), WalletError<StorageFault>>;

    /// # Errors
    ///
    /// 没有该币种的余额行时返回 `WalletError::Storage(StorageFault::MissingBalance)`。
    fn balance(
        &self,
        account: &AccountId,
        currency: Currency,
    ) -> Result<Money, WalletError<StorageFault>>;

    fn entries(&self) -> Vec<Entry>;

    /// # Errors
    ///
    /// 合计溢出时返回 `WalletError::Rejected(BillingRejection::Overflow)`。
    fn funds(&self, currency: Currency) -> Result<CurrencyFunds, WalletError<StorageFault>>;

    /// # Errors
    ///
    /// 没有未完成的冻结时返回 `WalletError::Rejected(BillingRejection::HoldNotFound)`。
    fn held(
        &self,
        account: &AccountId,
        request: &RequestId,
    ) -> Result<Money, WalletError<StorageFault>>;

    /// 已结算额只计 `since` 之后记到该币种 `Revenue` 的分录。当前未完成冻结全部计入，不按 `since` 截断。
    ///
    /// # Errors
    ///
    /// 金额相加溢出时返回 `WalletError::Rejected(BillingRejection::Overflow)`。
    fn spent(
        &self,
        key: &KeyId,
        currency: Currency,
        since: UnixMillis,
    ) -> Result<Money, WalletError<StorageFault>>;
}

/// 每个场景使用一个新的存储。Postgres 以后把同一个函数传给自己的构造器。
pub fn run_wallet_contract<S: WalletContract>(new_store: impl Fn() -> S) {
    missing_row_is_not_created(&new_store());
    hold_settle_and_release_keep_the_books(&new_store());
    repeated_terminal_calls_do_not_move_money(&new_store());
    cap_rejects_without_a_second_balance(&new_store());
    currencies_do_not_share_revenue(&new_store());
    concurrent_holds_cannot_both_pass_one_cap(&new_store());
    concurrent_holds_cannot_share_one_request(&new_store());
}

fn missing_row_is_not_created(store: &impl WalletContract) {
    let account = AccountId(String::from("missing"));
    let request = RequestId(String::from("missing-req"));
    let key = KeyId(String::from("missing-key"));
    let before = store.entries();
    let result = store.hold(&account, &request, &key, None, usd(1));
    assert!(matches!(
        result,
        Err(WalletError::Storage(StorageFault::MissingBalance))
    ));
    assert_eq!(store.entries().len(), before.len());
    assert_books(store);
}

fn hold_settle_and_release_keep_the_books(store: &impl WalletContract) {
    let account = AccountId(String::from("main"));
    let key = KeyId(String::from("main-key"));
    let held = RequestId(String::from("hold-1"));
    let released = RequestId(String::from("hold-2"));
    must(store.deposit(&account, usd(100)));
    must(store.hold(&account, &held, &key, None, usd(40)));
    assert_eq!(available(store, &account), 60);
    must(store.settle(&account, &held, usd(25)));
    assert_eq!(available(store, &account), 75);
    assert!(request_held_flat(&store.entries(), &account, &held).is_ok());
    must(store.hold(&account, &released, &key, None, usd(10)));
    must(store.release(&account, &released));
    assert_eq!(available(store, &account), 75);
    assert!(request_held_flat(&store.entries(), &account, &released).is_ok());
    let funds = must(store.funds(Currency::Usd));
    assert_eq!(funds.revenue, 25);
    assert_eq!(funds.available + funds.held, funds.deposits - funds.revenue);
    assert_books(store);
}

fn repeated_terminal_calls_do_not_move_money(store: &impl WalletContract) {
    let account = AccountId(String::from("repeat"));
    let key = KeyId(String::from("repeat-key"));
    let request = RequestId(String::from("repeat-req"));
    must(store.deposit(&account, usd(50)));
    must(store.hold(&account, &request, &key, None, usd(20)));
    let again = store.hold(&account, &request, &key, None, usd(20));
    assert!(matches!(
        again,
        Err(WalletError::Rejected(BillingRejection::DuplicateRequest))
    ));
    must(store.settle(&account, &request, usd(20)));
    let settled = store.settle(&account, &request, usd(20));
    assert!(matches!(
        settled,
        Err(WalletError::Rejected(BillingRejection::AlreadySettled))
    ));
    let released = store.release(&account, &request);
    assert!(matches!(
        released,
        Err(WalletError::Rejected(BillingRejection::AlreadySettled))
    ));
    assert_eq!(available(store, &account), 30);
    assert_books(store);

    let other = RequestId(String::from("repeat-release"));
    must(store.hold(&account, &other, &key, None, usd(5)));
    must(store.release(&account, &other));
    let twice = store.release(&account, &other);
    assert!(matches!(
        twice,
        Err(WalletError::Rejected(BillingRejection::AlreadyReleased))
    ));
    let settle_after = store.settle(&account, &other, usd(5));
    assert!(matches!(
        settle_after,
        Err(WalletError::Rejected(BillingRejection::AlreadyReleased))
    ));
    assert_books(store);
}

fn cap_rejects_without_a_second_balance(store: &impl WalletContract) {
    let account = AccountId(String::from("cap"));
    let key = KeyId(String::from("cap-key"));
    let request = RequestId(String::from("cap-req"));
    must(store.deposit(&account, usd(1_000)));
    let cap = KeyCap {
        since: UnixMillis(0),
        limit: usd(100),
    };
    let over = store.hold(&account, &request, &key, Some(cap), usd(101));
    assert!(matches!(
        over,
        Err(WalletError::Rejected(BillingRejection::LimitExceeded))
    ));
    assert_eq!(available(store, &account), 1_000);
    must(store.hold(&account, &request, &key, Some(cap), usd(40)));
    assert_eq!(available(store, &account), 960);
    assert_books(store);
}

fn currencies_do_not_share_revenue(store: &impl WalletContract) {
    let account = AccountId(String::from("fx"));
    let key = KeyId(String::from("fx-key"));
    let request = RequestId(String::from("fx-req"));
    must(store.deposit(&account, usd(80)));
    must(store.deposit(&account, cny(70)));
    must(store.hold(&account, &request, &key, None, usd(30)));
    must(store.settle(&account, &request, usd(30)));
    let usd_funds = must(store.funds(Currency::Usd));
    let cny_funds = must(store.funds(Currency::Cny));
    assert_eq!(usd_funds.revenue, 30);
    assert_eq!(cny_funds.revenue, 0);
    assert_eq!(cny_funds.available, 70);
    assert_books(store);
}

fn concurrent_holds_cannot_both_pass_one_cap(store: &impl WalletContract) {
    let account = AccountId(String::from("race-cap"));
    let key = KeyId(String::from("race-key"));
    let left = RequestId(String::from("race-left"));
    let right = RequestId(String::from("race-right"));
    must(store.deposit(&account, usd(1_000)));
    let cap = KeyCap {
        since: UnixMillis(0),
        limit: usd(100),
    };
    let (left_result, right_result) = race(
        || store.hold(&account, &left, &key, Some(cap), usd(60)),
        || store.hold(&account, &right, &key, Some(cap), usd(60)),
    );
    assert_eq!(successes(&left_result, &right_result), 1);
    let lost = if left_result.is_err() {
        left_result
    } else {
        right_result
    };
    assert!(matches!(
        lost,
        Err(WalletError::Rejected(BillingRejection::LimitExceeded))
    ));
    assert_eq!(available(store, &account), 940);
    assert_books(store);
}

fn concurrent_holds_cannot_share_one_request(store: &impl WalletContract) {
    let account = AccountId(String::from("race-req"));
    let key = KeyId(String::from("race-req-key"));
    let request = RequestId(String::from("same-request"));
    must(store.deposit(&account, usd(100)));
    let (left_result, right_result) = race(
        || store.hold(&account, &request, &key, None, usd(30)),
        || store.hold(&account, &request, &key, None, usd(30)),
    );
    assert_eq!(successes(&left_result, &right_result), 1);
    let lost = if left_result.is_err() {
        left_result
    } else {
        right_result
    };
    assert!(matches!(
        lost,
        Err(WalletError::Rejected(BillingRejection::DuplicateRequest))
    ));
    assert_eq!(available(store, &account), 70);
    assert_books(store);
}

fn race<T: Send>(left: impl FnOnce() -> T + Send, right: impl FnOnce() -> T + Send) -> (T, T) {
    let barrier = Arc::new(Barrier::new(2));
    let left_gate = Arc::clone(&barrier);
    let right_gate = barrier;
    thread::scope(|scope| {
        let left_handle = scope.spawn(move || {
            left_gate.wait();
            left()
        });
        let right_handle = scope.spawn(move || {
            right_gate.wait();
            right()
        });
        (joined(left_handle.join()), joined(right_handle.join()))
    })
}

fn successes(
    left: &Result<(), WalletError<StorageFault>>,
    right: &Result<(), WalletError<StorageFault>>,
) -> u8 {
    let mut count = 0_u8;
    if left.is_ok() {
        count += 1;
    }
    if right.is_ok() {
        count += 1;
    }
    count
}

fn assert_books(store: &impl WalletContract) {
    let entries = store.entries();
    let mut funds = Vec::new();
    for currency in [Currency::Usd, Currency::Cny] {
        let row = must(store.funds(currency));
        let used = row.available > 0 || row.held > 0 || row.deposits > 0 || row.revenue > 0;
        let mentioned = entries
            .iter()
            .any(|entry| entry.amount.currency == currency);
        if used || mentioned {
            funds.push(row);
        }
    }
    assert!(check_ledger(&entries, &funds).is_ok());
}

fn available(store: &impl WalletContract, account: &AccountId) -> u64 {
    must(store.balance(account, Currency::Usd)).minor_units
}

fn usd(minor_units: u64) -> Money {
    Money {
        currency: Currency::Usd,
        minor_units,
    }
}

fn cny(minor_units: u64) -> Money {
    Money {
        currency: Currency::Cny,
        minor_units,
    }
}

fn must<T, E: core::fmt::Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => {
            let rendered = format!("{error:?}");
            assert!(rendered.is_empty(), "{rendered}");
            std::process::abort();
        }
    }
}

fn joined<T>(result: thread::Result<T>) -> T {
    if let Ok(value) = result {
        value
    } else {
        let panicked = true;
        assert!(!panicked, "thread panicked");
        std::process::abort();
    }
}

#[cfg(test)]
mod tests {
    use super::run_wallet_contract;
    use crate::MemoryWallet;

    #[test]
    fn memory_wallet_matches_the_contract() {
        run_wallet_contract(MemoryWallet::new);
    }
}

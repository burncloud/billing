use std::future::Future;
use std::sync::{Arc, Barrier};
use std::task::{Context, Poll, Waker};
use std::thread;

use billing::errors::{BillingRejection, WalletError};
use billing::model::{Entry, KeyCap};
use billing::ports::Wallet;
use billing_domain::{check_ledger, request_held_flat, CurrencyFunds, StorageFault};
use kernel::model::{AccountId, Currency, KeyId, Money, RequestId, UnixMillis};

/// 内存和以后的 Postgres 共用的钱包端口，外加测试钩子。
pub trait WalletContract: Wallet<Error = StorageFault> + Sync {
    /// 已有该币种的行时，把金额加进可用余额和存入额。没有行时新建，冻结额为 0。
    ///
    /// # Errors
    ///
    /// 相加溢出时返回 `WalletError::Rejected(BillingRejection::Overflow)`。
    fn deposit(&self, account: &AccountId, amount: Money) -> Result<(), WalletError<StorageFault>>;

    fn entries(&self) -> Vec<Entry>;

    /// # Errors
    ///
    /// 合计溢出时返回 `WalletError::Rejected(BillingRejection::Overflow)`。
    fn funds(&self, currency: Currency) -> Result<CurrencyFunds, WalletError<StorageFault>>;

    /// 之后写入的分录使用这个时刻。
    fn set_now(&self, now: UnixMillis);
}

/// 把 `Wallet` 返回的 future 跑完。
pub trait DriveWallet {
    fn run<T, F>(&self, future: F) -> T
    where
        F: Future<Output = T> + Send;
}

/// 轮询一次。内存实现的 future 都是 ready。
#[derive(Debug, Clone, Copy)]
pub struct PollReady;

impl DriveWallet for PollReady {
    fn run<T, F>(&self, future: F) -> T
    where
        F: Future<Output = T> + Send,
    {
        let mut future = std::pin::pin!(future);
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => value,
            Poll::Pending => unreachable!("内存钱包的 future 不会 pending"),
        }
    }
}

/// 每个场景使用一个新的存储。`drive` 由调用方提供：内存传 [`PollReady`]，Postgres 传运行时的 `block_on`。
pub fn run_wallet_contract<S, D>(new_store: impl Fn() -> S, drive: &D)
where
    S: WalletContract,
    D: DriveWallet + Sync,
{
    missing_row_is_not_created(&new_store(), drive);
    hold_settle_and_release_keep_the_books(&new_store(), drive);
    repeated_terminal_calls_do_not_move_money(&new_store(), drive);
    cap_rejects_without_a_second_balance(&new_store(), drive);
    settled_spend_respects_since(&new_store(), drive);
    currencies_do_not_share_revenue(&new_store(), drive);
    concurrent_holds_cannot_both_pass_one_cap(&new_store(), drive);
    concurrent_holds_cannot_share_one_request(&new_store(), drive);
}

fn missing_row_is_not_created<S: WalletContract, D: DriveWallet>(store: &S, drive: &D) {
    let account = AccountId(String::from("missing"));
    let request = RequestId(String::from("missing-req"));
    let key = KeyId(String::from("missing-key"));
    let before = store.entries();
    let result = drive.run(Wallet::hold(store, &account, &request, &key, None, usd(1)));
    assert!(matches!(
        result,
        Err(WalletError::Storage(StorageFault::MissingBalance))
    ));
    assert_eq!(store.entries().len(), before.len());
    assert_books(store);
    assert!(matches!(
        drive.run(Wallet::balance(store, &account, Currency::Usd)),
        Err(WalletError::Storage(StorageFault::MissingBalance))
    ));
}

fn hold_settle_and_release_keep_the_books<S: WalletContract, D: DriveWallet>(store: &S, drive: &D) {
    let account = AccountId(String::from("main"));
    let key = KeyId(String::from("main-key"));
    let held = RequestId(String::from("hold-1"));
    let released = RequestId(String::from("hold-2"));
    must(store.deposit(&account, usd(100)));
    must(drive.run(Wallet::hold(store, &account, &held, &key, None, usd(40))));
    assert_eq!(available(store, drive, &account), 60);
    must(drive.run(Wallet::settle(store, &account, &held, usd(25))));
    assert_eq!(available(store, drive, &account), 75);
    assert!(request_held_flat(&store.entries(), &account, &held).is_ok());
    must(drive.run(Wallet::hold(
        store,
        &account,
        &released,
        &key,
        None,
        usd(10),
    )));
    must(drive.run(Wallet::release(store, &account, &released)));
    assert_eq!(available(store, drive, &account), 75);
    assert!(request_held_flat(&store.entries(), &account, &released).is_ok());
    let funds = must(store.funds(Currency::Usd));
    assert_eq!(funds.revenue, 25);
    assert_eq!(funds.available + funds.held, funds.deposits - funds.revenue);
    assert_books(store);
}

fn repeated_terminal_calls_do_not_move_money<S: WalletContract, D: DriveWallet>(
    store: &S,
    drive: &D,
) {
    let account = AccountId(String::from("repeat"));
    let key = KeyId(String::from("repeat-key"));
    let request = RequestId(String::from("repeat-req"));
    must(store.deposit(&account, usd(50)));
    must(drive.run(Wallet::hold(store, &account, &request, &key, None, usd(20))));
    let again = drive.run(Wallet::hold(store, &account, &request, &key, None, usd(20)));
    assert!(matches!(
        again,
        Err(WalletError::Rejected(BillingRejection::DuplicateRequest))
    ));
    must(drive.run(Wallet::settle(store, &account, &request, usd(20))));
    let settled = drive.run(Wallet::settle(store, &account, &request, usd(20)));
    assert!(matches!(
        settled,
        Err(WalletError::Rejected(BillingRejection::AlreadySettled))
    ));
    let released = drive.run(Wallet::release(store, &account, &request));
    assert!(matches!(
        released,
        Err(WalletError::Rejected(BillingRejection::AlreadySettled))
    ));
    assert_eq!(available(store, drive, &account), 30);
    assert_books(store);

    let other = RequestId(String::from("repeat-release"));
    must(drive.run(Wallet::hold(store, &account, &other, &key, None, usd(5))));
    must(drive.run(Wallet::release(store, &account, &other)));
    let twice = drive.run(Wallet::release(store, &account, &other));
    assert!(matches!(
        twice,
        Err(WalletError::Rejected(BillingRejection::AlreadyReleased))
    ));
    let settle_after = drive.run(Wallet::settle(store, &account, &other, usd(5)));
    assert!(matches!(
        settle_after,
        Err(WalletError::Rejected(BillingRejection::AlreadyReleased))
    ));
    assert_books(store);
}

fn cap_rejects_without_a_second_balance<S: WalletContract, D: DriveWallet>(store: &S, drive: &D) {
    let account = AccountId(String::from("cap"));
    let key = KeyId(String::from("cap-key"));
    let request = RequestId(String::from("cap-req"));
    must(store.deposit(&account, usd(1_000)));
    let cap = KeyCap {
        since: UnixMillis(0),
        limit: usd(100),
    };
    let over = drive.run(Wallet::hold(
        store,
        &account,
        &request,
        &key,
        Some(cap),
        usd(101),
    ));
    assert!(matches!(
        over,
        Err(WalletError::Rejected(BillingRejection::LimitExceeded))
    ));
    assert_eq!(available(store, drive, &account), 1_000);
    must(drive.run(Wallet::hold(
        store,
        &account,
        &request,
        &key,
        Some(cap),
        usd(40),
    )));
    assert_eq!(available(store, drive, &account), 960);
    assert_books(store);
}

fn settled_spend_respects_since<S: WalletContract, D: DriveWallet>(store: &S, drive: &D) {
    let account = AccountId(String::from("since"));
    let key = KeyId(String::from("since-key"));
    let settled = RequestId(String::from("since-settled"));
    let blocked = RequestId(String::from("since-blocked"));
    let allowed = RequestId(String::from("since-allowed"));
    store.set_now(UnixMillis(10));
    must(store.deposit(&account, usd(200)));
    must(drive.run(Wallet::hold(store, &account, &settled, &key, None, usd(60))));
    must(drive.run(Wallet::settle(store, &account, &settled, usd(60))));
    let inside = must(drive.run(Wallet::spent(store, &key, Currency::Usd, UnixMillis(5))));
    let outside = must(drive.run(Wallet::spent(store, &key, Currency::Usd, UnixMillis(20))));
    assert_eq!(inside.minor_units, 60);
    assert_eq!(outside.minor_units, 0);
    let tight = KeyCap {
        since: UnixMillis(5),
        limit: usd(100),
    };
    let over = drive.run(Wallet::hold(
        store,
        &account,
        &blocked,
        &key,
        Some(tight),
        usd(50),
    ));
    assert!(matches!(
        over,
        Err(WalletError::Rejected(BillingRejection::LimitExceeded))
    ));
    let wide = KeyCap {
        since: UnixMillis(20),
        limit: usd(100),
    };
    must(drive.run(Wallet::hold(
        store,
        &account,
        &allowed,
        &key,
        Some(wide),
        usd(50),
    )));
    let with_hold_inside =
        must(drive.run(Wallet::spent(store, &key, Currency::Usd, UnixMillis(5))));
    let with_hold_outside =
        must(drive.run(Wallet::spent(store, &key, Currency::Usd, UnixMillis(20))));
    assert_eq!(with_hold_inside.minor_units, 110);
    assert_eq!(with_hold_outside.minor_units, 50);
    assert_books(store);
}

fn currencies_do_not_share_revenue<S: WalletContract, D: DriveWallet>(store: &S, drive: &D) {
    let account = AccountId(String::from("fx"));
    let key = KeyId(String::from("fx-key"));
    let request = RequestId(String::from("fx-req"));
    must(store.deposit(&account, usd(80)));
    must(store.deposit(&account, cny(70)));
    must(drive.run(Wallet::hold(store, &account, &request, &key, None, usd(30))));
    must(drive.run(Wallet::settle(store, &account, &request, usd(30))));
    let usd_funds = must(store.funds(Currency::Usd));
    let cny_funds = must(store.funds(Currency::Cny));
    assert_eq!(usd_funds.revenue, 30);
    assert_eq!(cny_funds.revenue, 0);
    assert_eq!(cny_funds.available, 70);
    assert_books(store);
}

fn concurrent_holds_cannot_both_pass_one_cap<S: WalletContract, D: DriveWallet + Sync>(
    store: &S,
    drive: &D,
) {
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
        || {
            drive.run(Wallet::hold(
                store,
                &account,
                &left,
                &key,
                Some(cap),
                usd(60),
            ))
        },
        || {
            drive.run(Wallet::hold(
                store,
                &account,
                &right,
                &key,
                Some(cap),
                usd(60),
            ))
        },
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
    assert_eq!(available(store, drive, &account), 940);
    assert_books(store);
}

fn concurrent_holds_cannot_share_one_request<S: WalletContract, D: DriveWallet + Sync>(
    store: &S,
    drive: &D,
) {
    let account = AccountId(String::from("race-req"));
    let key = KeyId(String::from("race-req-key"));
    let request = RequestId(String::from("same-request"));
    must(store.deposit(&account, usd(100)));
    let (left_result, right_result) = race(
        || drive.run(Wallet::hold(store, &account, &request, &key, None, usd(30))),
        || drive.run(Wallet::hold(store, &account, &request, &key, None, usd(30))),
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
    assert_eq!(available(store, drive, &account), 70);
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

fn available<S: WalletContract, D: DriveWallet>(store: &S, drive: &D, account: &AccountId) -> u64 {
    must(drive.run(Wallet::balance(store, account, Currency::Usd))).minor_units
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
        Err(error) => unreachable!("{error:?}"),
    }
}

fn joined<T>(result: thread::Result<T>) -> T {
    match result {
        Ok(value) => value,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[cfg(test)]
mod tests {
    use super::{run_wallet_contract, PollReady};
    use crate::{ManualClock, MemoryWallet};
    use kernel::model::UnixMillis;

    #[test]
    fn memory_wallet_matches_the_contract() {
        run_wallet_contract(
            || MemoryWallet::new(ManualClock::new(UnixMillis(0))),
            &PollReady,
        );
    }
}

use std::future::Future;
use std::sync::{Arc, Barrier};
use std::task::{Context, Poll, Waker};
use std::thread;

use billing::errors::{BillingRejection, WalletError};
use billing::model::{Direction, Entry, EntryRef, KeyCap, LedgerAccount};
use billing::ports::Wallet;
use billing_domain::{
    check_ledger, deposits_in, request_held_flat, AccountFunds, CurrencyFunds, StorageFault,
};
use kernel::model::{AccountId, Currency, KeyId, Money, RequestId, UnixMillis};

/// 内存和以后的 Postgres 共用的钱包端口，外加测试钩子。
pub trait WalletContract: Wallet<Error = StorageFault> + Sync {
    /// 已有该币种的行时，把金额加进可用余额和存入额。没有行时新建，冻结额为 0。
    /// 同时写入入账分录：借 `Funding`、贷 `Available(account)`，单号由钱包自己生成。
    ///
    /// # Errors
    ///
    /// 相加溢出时返回 `WalletError::Rejected(BillingRejection::Overflow)`。
    fn deposit(&self, account: &AccountId, amount: Money) -> Result<(), WalletError<StorageFault>>;

    fn entries(&self) -> Vec<Entry>;

    /// 为 `(account, currency)` 建一行余额为 0 的记录，已存在就不动。
    ///
    /// # Errors
    ///
    /// 币种不是 USD 时返回 `FundingRejection::UnsupportedCurrency`。存储失败时返回
    /// `FundingError::Storage`。
    fn open_account(
        &self,
        account: &AccountId,
        currency: Currency,
    ) -> Result<billing_domain::Opened, billing_domain::FundingError<StorageFault>>;

    /// 管理员入账。同一账户的同一单号只记一次。
    ///
    /// # Errors
    ///
    /// 拒绝原因见 `billing_domain::post_credit`。存储失败时返回 `billing_domain::FundingError::Storage`。
    fn credit(
        &self,
        account: &AccountId,
        credit: &billing_domain::CreditId,
        amount: Money,
    ) -> Result<billing_domain::Credited, billing_domain::FundingError<StorageFault>>;

    /// 按 `created_at`、账户、请求排序。
    ///
    /// # Errors
    ///
    /// 存储失败时返回 `WalletError::Storage`。
    fn open_holds_created_before(
        &self,
        cutoff: UnixMillis,
    ) -> Result<Vec<billing_domain::PendingHold>, WalletError<StorageFault>>;

    /// # Errors
    ///
    /// 合计溢出时返回 `WalletError::Rejected(BillingRejection::Overflow)`。
    fn funds(&self, currency: Currency) -> Result<CurrencyFunds, WalletError<StorageFault>>;

    /// 钱包表里每个账户每个币种的一行，按账户、币种排序。
    fn account_funds(&self) -> Vec<AccountFunds>;

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
    concurrent_accounts_share_one_key_cap(&new_store(), drive);
    accounts_open_and_credits_apply_once(&new_store(), drive);
    only_usd_accounts_are_opened_and_funded(&new_store(), drive);
    credits_write_one_ledger_pair_once(&new_store(), drive);
    deposits_and_credits_share_the_ledger(&new_store(), drive);
    open_holds_carry_their_creation_time(&new_store(), drive);
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
    let deposits = must(deposits_in(&store.entries(), Currency::Usd));
    assert_eq!(deposits, 100);
    assert_eq!(funds.available + funds.held, deposits - funds.revenue);
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

fn concurrent_accounts_share_one_key_cap<S: WalletContract, D: DriveWallet + Sync>(
    store: &S,
    drive: &D,
) {
    let left_account = AccountId(String::from("race-cap-a"));
    let right_account = AccountId(String::from("race-cap-b"));
    let key = KeyId(String::from("race-shared-key"));
    let left = RequestId(String::from("race-a"));
    let right = RequestId(String::from("race-b"));
    must(store.deposit(&left_account, usd(1_000)));
    must(store.deposit(&right_account, usd(1_000)));
    let cap = KeyCap {
        since: UnixMillis(0),
        limit: usd(100),
    };
    let (left_result, right_result) = race(
        || {
            drive.run(Wallet::hold(
                store,
                &left_account,
                &left,
                &key,
                Some(cap),
                usd(60),
            ))
        },
        || {
            drive.run(Wallet::hold(
                store,
                &right_account,
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
    let left_available = must(drive.run(Wallet::balance(store, &left_account, Currency::Usd)));
    let right_available = must(drive.run(Wallet::balance(store, &right_account, Currency::Usd)));
    let mut balances = [left_available.minor_units, right_available.minor_units];
    balances.sort_unstable();
    assert_eq!(balances, [940, 1_000]);
    assert_books(store);
}

fn accounts_open_and_credits_apply_once<S: WalletContract, D: DriveWallet>(store: &S, drive: &D) {
    use billing_domain::{CreditId, Credited, FundingError, FundingRejection, Opened};

    let account = AccountId(String::from("fund"));
    let request = RequestId(String::from("fund-req"));
    let key = KeyId(String::from("fund-key"));
    let first = CreditId(String::from("credit-1"));
    assert!(matches!(
        drive.run(Wallet::hold(store, &account, &request, &key, None, usd(1))),
        Err(WalletError::Storage(StorageFault::MissingBalance))
    ));
    assert_eq!(
        store.credit(&account, &first, usd(100)),
        Err(FundingError::Rejected(FundingRejection::NotOpened))
    );
    assert_eq!(
        must(store.open_account(&account, Currency::Usd)),
        Opened::Created
    );
    assert_eq!(
        must(store.open_account(&account, Currency::Usd)),
        Opened::AlreadyOpen
    );
    assert_eq!(available(store, drive, &account), 0);
    assert!(matches!(
        drive.run(Wallet::hold(store, &account, &request, &key, None, usd(1))),
        Err(WalletError::Rejected(BillingRejection::InsufficientBalance))
    ));
    assert_eq!(
        store.credit(&account, &first, usd(100)),
        Ok(Credited::Applied)
    );
    assert_eq!(
        store.credit(&account, &first, usd(100)),
        Ok(Credited::AlreadyApplied)
    );
    assert_eq!(available(store, drive, &account), 100);
    assert_eq!(
        store.credit(&account, &first, usd(50)),
        Err(FundingError::Rejected(FundingRejection::ConflictingCredit))
    );
    assert_eq!(
        store.credit(&account, &CreditId(String::from("credit-0")), usd(0)),
        Err(FundingError::Rejected(FundingRejection::ZeroAmount))
    );
    assert_eq!(
        store.credit(&account, &CreditId(String::new()), usd(1)),
        Err(FundingError::Rejected(FundingRejection::EmptyCreditId))
    );
    assert_eq!(
        store.credit(
            &account,
            &CreditId(String::from("credit-big")),
            usd(u64::MAX)
        ),
        Err(FundingError::Rejected(FundingRejection::Overflow))
    );
    must(drive.run(Wallet::hold(store, &account, &request, &key, None, usd(30))));
    assert_eq!(
        store.credit(&account, &CreditId(String::from("credit-2")), usd(5)),
        Ok(Credited::Applied)
    );
    assert_eq!(available(store, drive, &account), 75);
    assert_eq!(
        must(drive.run(Wallet::held(store, &account, &request))),
        usd(30)
    );
    assert_eq!(
        must(store.open_account(&account, Currency::Usd)),
        Opened::AlreadyOpen
    );
    assert_eq!(available(store, drive, &account), 75);
    let other = AccountId(String::from("fund-other"));
    assert_eq!(
        must(store.open_account(&other, Currency::Usd)),
        Opened::Created
    );
    assert_eq!(store.credit(&other, &first, usd(7)), Ok(Credited::Applied));
    assert_eq!(available(store, drive, &other), 7);
    let funds = must(store.funds(Currency::Usd));
    assert_eq!(must(deposits_in(&store.entries(), Currency::Usd)), 112);
    assert_eq!(funds.available + funds.held, 112);
    assert_books(store);
}

fn only_usd_accounts_are_opened_and_funded<S: WalletContract, D: DriveWallet>(
    store: &S,
    drive: &D,
) {
    use billing_domain::{CreditId, Credited, FundingError, FundingRejection, Opened};

    let account = AccountId(String::from("usd-only"));
    let first = CreditId(String::from("credit-1"));
    let unsupported = FundingRejection::UnsupportedCurrency;
    assert_eq!(
        store.open_account(&account, Currency::Cny),
        Err(FundingError::Rejected(unsupported))
    );
    assert!(matches!(
        drive.run(Wallet::balance(store, &account, Currency::Cny)),
        Err(WalletError::Storage(StorageFault::MissingBalance))
    ));
    assert_eq!(
        must(store.open_account(&account, Currency::Usd)),
        Opened::Created
    );
    assert_eq!(
        store.credit(&account, &first, usd(100)),
        Ok(Credited::Applied)
    );
    assert_eq!(
        store.credit(&account, &first, cny(100)),
        Err(FundingError::Rejected(unsupported))
    );
    assert_eq!(
        store.credit(&account, &CreditId(String::from("credit-cny")), cny(5)),
        Err(FundingError::Rejected(unsupported))
    );
    assert_eq!(available(store, drive, &account), 100);
    assert_eq!(must(deposits_in(&store.entries(), Currency::Cny)), 0);
    assert_books(store);
}

fn credit_pair(
    account: &AccountId,
    id: &billing_domain::CreditId,
    minor: u64,
    at: u64,
) -> [Entry; 2] {
    let reference = EntryRef::Credit(id.clone());
    [
        Entry {
            reference: reference.clone(),
            account: LedgerAccount::Funding,
            key: None,
            direction: Direction::Debit,
            amount: usd(minor),
            at: UnixMillis(at),
        },
        Entry {
            reference,
            account: LedgerAccount::Available(account.clone()),
            key: None,
            direction: Direction::Credit,
            amount: usd(minor),
            at: UnixMillis(at),
        },
    ]
}

fn credits_write_one_ledger_pair_once<S: WalletContract, D: DriveWallet>(store: &S, drive: &D) {
    use billing_domain::{CreditId, Credited, FundingError, FundingRejection};

    let account = AccountId(String::from("ledger"));
    let other = AccountId(String::from("ledger-other"));
    let first = CreditId(String::from("ledger-1"));
    must(store.open_account(&account, Currency::Usd));
    must(store.open_account(&other, Currency::Usd));
    assert_eq!(store.entries().len(), 0);
    assert!(store.credit(&account, &first, usd(0)).is_err());
    assert_eq!(store.entries().len(), 0);
    store.set_now(UnixMillis(500));
    assert_eq!(
        store.credit(&account, &first, usd(100)),
        Ok(Credited::Applied)
    );
    let mut expected: Vec<Entry> = credit_pair(&account, &first, 100, 500).to_vec();
    assert_eq!(store.entries(), expected);
    assert_books(store);

    store.set_now(UnixMillis(600));
    assert_eq!(
        store.credit(&account, &first, usd(100)),
        Ok(Credited::AlreadyApplied)
    );
    assert_eq!(store.entries(), expected);
    assert_eq!(
        store.credit(&account, &first, usd(7)),
        Err(FundingError::Rejected(FundingRejection::ConflictingCredit))
    );
    assert_eq!(
        store.credit(&account, &first, cny(100)),
        Err(FundingError::Rejected(
            FundingRejection::UnsupportedCurrency
        ))
    );
    assert_eq!(
        store.credit(&account, &CreditId(String::new()), usd(1)),
        Err(FundingError::Rejected(FundingRejection::EmptyCreditId))
    );
    assert_eq!(
        store.credit(
            &account,
            &CreditId(String::from("ledger-big")),
            usd(u64::MAX)
        ),
        Err(FundingError::Rejected(FundingRejection::Overflow))
    );
    assert_eq!(store.entries(), expected);
    assert_books(store);

    assert_eq!(store.credit(&other, &first, usd(30)), Ok(Credited::Applied));
    expected.extend(credit_pair(&other, &first, 30, 600));
    assert_eq!(store.entries(), expected);
    assert_eq!(must(deposits_in(&store.entries(), Currency::Usd)), 130);
    assert_books(store);

    let request = RequestId(String::from("ledger-req"));
    let key = KeyId(String::from("ledger-key"));
    must(drive.run(Wallet::hold(store, &account, &request, &key, None, usd(40))));
    must(drive.run(Wallet::settle(store, &account, &request, usd(25))));
    assert_eq!(available(store, drive, &account), 75);
    assert_eq!(available(store, drive, &other), 30);
    assert_eq!(
        store.credit(&account, &first, usd(100)),
        Ok(Credited::AlreadyApplied)
    );
    assert_eq!(store.entries().len(), expected.len() + 6);
    assert_books(store);
}

fn deposits_and_credits_share_the_ledger<S: WalletContract, D: DriveWallet>(store: &S, drive: &D) {
    use billing_domain::{CreditId, Credited};

    let account = AccountId(String::from("shared"));
    must(store.deposit(&account, usd(10)));
    must(store.deposit(&account, cny(4)));
    assert_eq!(
        store.credit(&account, &CreditId(String::from("shared-1")), usd(5)),
        Ok(Credited::Applied)
    );
    let entries = store.entries();
    assert_eq!(entries.len(), 6);
    assert_eq!(must(deposits_in(&entries, Currency::Usd)), 15);
    assert_eq!(must(deposits_in(&entries, Currency::Cny)), 4);
    assert_eq!(available(store, drive, &account), 15);
    assert_books(store);
}

fn open_holds_carry_their_creation_time<S: WalletContract, D: DriveWallet>(store: &S, drive: &D) {
    use billing_domain::PendingHold;

    let account = AccountId(String::from("aged"));
    let key = KeyId(String::from("aged-key"));
    let early = RequestId(String::from("aged-early"));
    let late = RequestId(String::from("aged-late"));
    let closed = RequestId(String::from("aged-closed"));
    must(store.deposit(&account, usd(100)));
    store.set_now(UnixMillis(100));
    must(drive.run(Wallet::hold(store, &account, &early, &key, None, usd(10))));
    must(drive.run(Wallet::hold(store, &account, &closed, &key, None, usd(10))));
    store.set_now(UnixMillis(200));
    must(drive.run(Wallet::hold(store, &account, &late, &key, None, usd(10))));
    must(drive.run(Wallet::release(store, &account, &closed)));
    let before_late = must(store.open_holds_created_before(UnixMillis(200)));
    assert_eq!(
        before_late,
        vec![PendingHold {
            account: account.clone(),
            request: early.clone(),
            key: key.clone(),
            amount: usd(10),
            created_at: UnixMillis(100),
        }]
    );
    let all = must(store.open_holds_created_before(UnixMillis(201)));
    let requests: Vec<(&str, u64)> = all
        .iter()
        .map(|hold| (hold.request.0.as_str(), hold.created_at.0))
        .collect();
    assert_eq!(requests, [("aged-early", 100), ("aged-late", 200)]);
    assert_eq!(must(store.open_holds_created_before(UnixMillis(100))), []);
    must(drive.run(Wallet::settle(store, &account, &early, usd(10))));
    let after_settle = must(store.open_holds_created_before(UnixMillis(201)));
    assert_eq!(after_settle.len(), 1);
    assert_eq!(after_settle[0].request, late);
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
        let used = row.available > 0 || row.held > 0 || row.revenue > 0;
        let mentioned = entries
            .iter()
            .any(|entry| entry.amount.currency == currency);
        if used || mentioned {
            funds.push(row);
        }
    }
    assert!(check_ledger(&entries, &funds, &store.account_funds()).is_ok());
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

use billing::errors::{BillingRejection, WalletError};
use billing::model::{Direction, Entry, EntryRef, KeyCap, LedgerAccount};
use kernel::model::{AccountId, Currency, KeyId, Money, RequestId, UnixMillis};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalletSnapshot {
    pub available: Money,
    pub held: Money,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenHold {
    pub key: KeyId,
    pub amount: Money,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExistingHold {
    Open(OpenHold),
    Settled,
    Released,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpendSoFar {
    pub settled: Money,
    pub open: Money,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageFault {
    MissingBalance,
    HeldBelowOpen,
}

/// 金额为 0 的分录不写进 `entries`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posted {
    pub entries: Vec<Entry>,
    pub balance: WalletSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldRequest<'a> {
    pub account: &'a AccountId,
    pub request: &'a RequestId,
    pub key: &'a KeyId,
    pub cap: Option<KeyCap>,
    pub amount: Money,
    pub at: UnixMillis,
    pub balance: Option<WalletSnapshot>,
    pub existing: Option<ExistingHold>,
    pub spent: SpendSoFar,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettleRequest<'a> {
    pub account: &'a AccountId,
    pub request: &'a RequestId,
    pub amount: Money,
    pub at: UnixMillis,
    pub balance: WalletSnapshot,
    pub existing: Option<ExistingHold>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseRequest<'a> {
    pub account: &'a AccountId,
    pub request: &'a RequestId,
    pub at: UnixMillis,
    pub balance: WalletSnapshot,
    pub existing: Option<ExistingHold>,
}

/// `existing` 为 `Some` 时，该 `(account, request)` 曾经冻结过，含已结算和已释放。
/// `cap` 为 `None` 时不检查限额，也不读取 `spent`。`cap.since` 已由调用方折进 `spent`。
///
/// # Errors
///
/// `existing` 为 `Some` 时返回 `WalletError::Rejected(BillingRejection::DuplicateRequest)`。
/// `balance` 为 `None` 时返回 `WalletError::Storage(StorageFault::MissingBalance)`，不创建余额行。
/// 余额、限额或 `spent` 的币种与 `amount` 不同时返回 `WalletError::Rejected(BillingRejection::CurrencyMismatch)`。
/// `spent.settled + spent.open + amount` 超过 `cap.limit` 时返回 `WalletError::Rejected(BillingRejection::LimitExceeded)`。
/// 可用余额小于 `amount` 时返回 `WalletError::Rejected(BillingRejection::InsufficientBalance)`。
/// 上述加法溢出时返回 `WalletError::Rejected(BillingRejection::Overflow)`。
pub fn post_hold(request: &HoldRequest<'_>) -> Result<Posted, WalletError<StorageFault>> {
    if request.existing.is_some() {
        return Err(rejected(BillingRejection::DuplicateRequest));
    }
    let Some(balance) = request.balance else {
        return Err(WalletError::Storage(StorageFault::MissingBalance));
    };
    let currency = snapshot_currency(balance, request.amount)?;
    if let Some(cap) = request.cap {
        check_cap(cap, request.spent, request.amount)?;
    }
    let available = balance
        .available
        .minor_units
        .checked_sub(request.amount.minor_units)
        .ok_or(rejected(BillingRejection::InsufficientBalance))?;
    let held = balance
        .held
        .minor_units
        .checked_add(request.amount.minor_units)
        .ok_or(rejected(BillingRejection::Overflow))?;
    let mut entries = Vec::new();
    push_pair(
        &mut entries,
        &EntryRef::Request(request.request.clone()),
        Some(request.key.clone()),
        request.amount,
        request.at,
        LedgerAccount::Available(request.account.clone()),
        LedgerAccount::Held(request.account.clone()),
    );
    Ok(Posted {
        entries,
        balance: snapshot(currency, available, held),
    })
}

/// 调用方先把实际金额封顶到冻结额。超额不改小。
///
/// # Errors
///
/// 没有冻结时返回 `WalletError::Rejected(BillingRejection::HoldNotFound)`。
/// 已结算时返回 `WalletError::Rejected(BillingRejection::AlreadySettled)`。
/// 已释放时返回 `WalletError::Rejected(BillingRejection::AlreadyReleased)`。
/// `amount.currency` 或余额币种与冻结币种不同时返回 `WalletError::Rejected(BillingRejection::CurrencyMismatch)`。
/// `amount` 大于冻结额时返回 `WalletError::Rejected(BillingRejection::SettlementExceedsHold)`。
/// 余额行上的冻结额小于这笔未完成冻结时返回 `WalletError::Storage(StorageFault::HeldBelowOpen)`。
/// 退回可用余额的加法溢出时返回 `WalletError::Rejected(BillingRejection::Overflow)`。
pub fn post_settle(request: &SettleRequest<'_>) -> Result<Posted, WalletError<StorageFault>> {
    let open = open_hold(request.existing.as_ref())?;
    let currency = snapshot_currency(request.balance, open.amount)?;
    if request.amount.currency != currency {
        return Err(rejected(BillingRejection::CurrencyMismatch));
    }
    if request.amount.minor_units > open.amount.minor_units {
        return Err(rejected(BillingRejection::SettlementExceedsHold));
    }
    let remainder = open.amount.minor_units - request.amount.minor_units;
    let held = request
        .balance
        .held
        .minor_units
        .checked_sub(open.amount.minor_units)
        .ok_or(WalletError::Storage(StorageFault::HeldBelowOpen))?;
    let available = request
        .balance
        .available
        .minor_units
        .checked_add(remainder)
        .ok_or(rejected(BillingRejection::Overflow))?;
    let mut entries = Vec::new();
    let key = Some(open.key.clone());
    push_pair(
        &mut entries,
        &EntryRef::Request(request.request.clone()),
        key.clone(),
        money(currency, request.amount.minor_units),
        request.at,
        LedgerAccount::Held(request.account.clone()),
        LedgerAccount::Revenue,
    );
    push_pair(
        &mut entries,
        &EntryRef::Request(request.request.clone()),
        key,
        money(currency, remainder),
        request.at,
        LedgerAccount::Held(request.account.clone()),
        LedgerAccount::Available(request.account.clone()),
    );
    Ok(Posted {
        entries,
        balance: snapshot(currency, available, held),
    })
}

/// # Errors
///
/// 没有冻结时返回 `WalletError::Rejected(BillingRejection::HoldNotFound)`。
/// 已释放时返回 `WalletError::Rejected(BillingRejection::AlreadyReleased)`。
/// 已结算时返回 `WalletError::Rejected(BillingRejection::AlreadySettled)`。
/// 余额币种与冻结币种不同时返回 `WalletError::Rejected(BillingRejection::CurrencyMismatch)`。
/// 余额行上的冻结额小于这笔未完成冻结时返回 `WalletError::Storage(StorageFault::HeldBelowOpen)`。
/// 退回可用余额的加法溢出时返回 `WalletError::Rejected(BillingRejection::Overflow)`。
pub fn post_release(request: &ReleaseRequest<'_>) -> Result<Posted, WalletError<StorageFault>> {
    let open = open_hold(request.existing.as_ref())?;
    let currency = snapshot_currency(request.balance, open.amount)?;
    let held = request
        .balance
        .held
        .minor_units
        .checked_sub(open.amount.minor_units)
        .ok_or(WalletError::Storage(StorageFault::HeldBelowOpen))?;
    let available = request
        .balance
        .available
        .minor_units
        .checked_add(open.amount.minor_units)
        .ok_or(rejected(BillingRejection::Overflow))?;
    let mut entries = Vec::new();
    push_pair(
        &mut entries,
        &EntryRef::Request(request.request.clone()),
        Some(open.key.clone()),
        open.amount,
        request.at,
        LedgerAccount::Held(request.account.clone()),
        LedgerAccount::Available(request.account.clone()),
    );
    Ok(Posted {
        entries,
        balance: snapshot(currency, available, held),
    })
}

fn open_hold(existing: Option<&ExistingHold>) -> Result<&OpenHold, WalletError<StorageFault>> {
    match existing {
        Some(ExistingHold::Open(open)) => Ok(open),
        Some(ExistingHold::Settled) => Err(rejected(BillingRejection::AlreadySettled)),
        Some(ExistingHold::Released) => Err(rejected(BillingRejection::AlreadyReleased)),
        None => Err(rejected(BillingRejection::HoldNotFound)),
    }
}

fn check_cap(
    cap: KeyCap,
    spent: SpendSoFar,
    amount: Money,
) -> Result<(), WalletError<StorageFault>> {
    if cap.limit.currency != amount.currency
        || spent.settled.currency != amount.currency
        || spent.open.currency != amount.currency
    {
        return Err(rejected(BillingRejection::CurrencyMismatch));
    }
    let used = spent
        .settled
        .minor_units
        .checked_add(spent.open.minor_units)
        .ok_or(rejected(BillingRejection::Overflow))?;
    let next = used
        .checked_add(amount.minor_units)
        .ok_or(rejected(BillingRejection::Overflow))?;
    if next > cap.limit.minor_units {
        return Err(rejected(BillingRejection::LimitExceeded));
    }
    Ok(())
}

fn snapshot_currency(
    balance: WalletSnapshot,
    amount: Money,
) -> Result<Currency, WalletError<StorageFault>> {
    if balance.available.currency != amount.currency || balance.held.currency != amount.currency {
        return Err(rejected(BillingRejection::CurrencyMismatch));
    }
    Ok(amount.currency)
}

pub(crate) fn push_pair(
    entries: &mut Vec<Entry>,
    reference: &EntryRef,
    key: Option<KeyId>,
    amount: Money,
    at: UnixMillis,
    debit: LedgerAccount,
    credit: LedgerAccount,
) {
    if amount.minor_units == 0 {
        return;
    }
    entries.push(Entry {
        reference: reference.clone(),
        account: debit,
        key: key.clone(),
        direction: Direction::Debit,
        amount,
        at,
    });
    entries.push(Entry {
        reference: reference.clone(),
        account: credit,
        key,
        direction: Direction::Credit,
        amount,
        at,
    });
}

fn snapshot(currency: Currency, available: u64, held: u64) -> WalletSnapshot {
    WalletSnapshot {
        available: money(currency, available),
        held: money(currency, held),
    }
}

fn money(currency: Currency, minor_units: u64) -> Money {
    Money {
        currency,
        minor_units,
    }
}

fn rejected(reason: BillingRejection) -> WalletError<StorageFault> {
    WalletError::Rejected(reason)
}

#[cfg(test)]
mod tests {
    use super::{
        post_hold, post_release, post_settle, ExistingHold, HoldRequest, OpenHold, ReleaseRequest,
        SettleRequest, SpendSoFar, StorageFault, WalletSnapshot,
    };

    fn must<T, E: core::fmt::Debug>(result: Result<T, E>) -> T {
        match result {
            Ok(value) => value,
            Err(error) => panic!("{error:?}"),
        }
    }
    use billing::errors::{BillingRejection, WalletError};
    use billing::model::{Direction, KeyCap, LedgerAccount};
    use kernel::model::{AccountId, Currency, KeyId, Money, RequestId, UnixMillis};

    fn ids() -> (AccountId, RequestId, KeyId) {
        (
            AccountId(String::from("acct")),
            RequestId(String::from("req")),
            KeyId(String::from("key")),
        )
    }

    fn money(minor_units: u64) -> Money {
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

    fn snapshot(available: u64, held: u64) -> WalletSnapshot {
        WalletSnapshot {
            available: money(available),
            held: money(held),
        }
    }

    fn spent(settled: u64, open: u64) -> SpendSoFar {
        SpendSoFar {
            settled: money(settled),
            open: money(open),
        }
    }

    fn hold<'a>(
        ids: (&'a AccountId, &'a RequestId, &'a KeyId),
        amount: u64,
        balance: Option<WalletSnapshot>,
        existing: Option<ExistingHold>,
        cap: Option<KeyCap>,
        spent: SpendSoFar,
    ) -> HoldRequest<'a> {
        HoldRequest {
            account: ids.0,
            request: ids.1,
            key: ids.2,
            cap,
            amount: money(amount),
            at: UnixMillis(10),
            balance,
            existing,
            spent,
        }
    }

    #[test]
    fn hold_debits_available_and_credits_held() {
        let (account, request, key) = ids();
        let posted = must(post_hold(&hold(
            (&account, &request, &key),
            40,
            Some(snapshot(100, 5)),
            None,
            None,
            spent(0, 0),
        )));
        assert_eq!(posted.balance.available.minor_units, 60);
        assert_eq!(posted.balance.held.minor_units, 45);
        assert_eq!(posted.entries.len(), 2);
        assert!(matches!(
            (
                &posted.entries[0].account,
                posted.entries[0].direction,
                posted.entries[0].amount.minor_units
            ),
            (LedgerAccount::Available(_), Direction::Debit, 40)
        ));
        assert!(matches!(
            (&posted.entries[1].account, posted.entries[1].direction),
            (LedgerAccount::Held(_), Direction::Credit)
        ));
        assert_eq!(
            posted.entries[0].key.as_ref().map(|item| item.0.as_str()),
            Some("key")
        );
    }

    #[test]
    fn missing_balance_is_storage_and_duplicate_wins_when_a_hold_exists() {
        let (account, request, key) = ids();
        let missing = post_hold(&hold(
            (&account, &request, &key),
            1,
            None,
            None,
            None,
            spent(0, 0),
        ));
        assert!(matches!(
            missing,
            Err(WalletError::Storage(StorageFault::MissingBalance))
        ));
        let open = ExistingHold::Open(OpenHold {
            key: key.clone(),
            amount: money(1),
        });
        let again = post_hold(&hold(
            (&account, &request, &key),
            1,
            None,
            Some(open),
            None,
            spent(0, 0),
        ));
        assert!(matches!(
            again,
            Err(WalletError::Rejected(BillingRejection::DuplicateRequest))
        ));
        for existing in [ExistingHold::Settled, ExistingHold::Released] {
            let posted = post_hold(&hold(
                (&account, &request, &key),
                1,
                Some(snapshot(10, 0)),
                Some(existing),
                None,
                spent(0, 0),
            ));
            assert!(matches!(
                posted,
                Err(WalletError::Rejected(BillingRejection::DuplicateRequest))
            ));
        }
    }

    #[test]
    fn cap_none_skips_the_limit_and_a_breach_or_currency_mismatch_rejects() {
        let (account, request, key) = ids();
        let uncapped = post_hold(&hold(
            (&account, &request, &key),
            5,
            Some(snapshot(10, 0)),
            None,
            None,
            spent(u64::MAX, u64::MAX),
        ));
        assert!(uncapped.is_ok());
        let cap = KeyCap {
            since: UnixMillis(0),
            limit: money(10),
        };
        let over = post_hold(&hold(
            (&account, &request, &key),
            4,
            Some(snapshot(10, 0)),
            None,
            Some(cap),
            spent(5, 2),
        ));
        assert!(matches!(
            over,
            Err(WalletError::Rejected(BillingRejection::LimitExceeded))
        ));
        let foreign = KeyCap {
            since: UnixMillis(0),
            limit: cny(100),
        };
        let mismatch = post_hold(&hold(
            (&account, &request, &key),
            1,
            Some(snapshot(10, 0)),
            None,
            Some(foreign),
            spent(0, 0),
        ));
        assert!(matches!(
            mismatch,
            Err(WalletError::Rejected(BillingRejection::CurrencyMismatch))
        ));
        let spent_mismatch = SpendSoFar {
            settled: cny(1),
            open: money(0),
        };
        let spent_error = post_hold(&hold(
            (&account, &request, &key),
            1,
            Some(snapshot(10, 0)),
            None,
            Some(cap),
            spent_mismatch,
        ));
        assert!(matches!(
            spent_error,
            Err(WalletError::Rejected(BillingRejection::CurrencyMismatch))
        ));
    }

    #[test]
    fn insufficient_balance_and_held_overflow() {
        let (account, request, key) = ids();
        let poor = post_hold(&hold(
            (&account, &request, &key),
            11,
            Some(snapshot(10, 0)),
            None,
            None,
            spent(0, 0),
        ));
        assert!(matches!(
            poor,
            Err(WalletError::Rejected(BillingRejection::InsufficientBalance))
        ));
        let overflow = post_hold(&hold(
            (&account, &request, &key),
            1,
            Some(snapshot(10, u64::MAX)),
            None,
            None,
            spent(0, 0),
        ));
        assert!(matches!(
            overflow,
            Err(WalletError::Rejected(BillingRejection::Overflow))
        ));
        let cap = KeyCap {
            since: UnixMillis(0),
            limit: money(u64::MAX),
        };
        let sum = post_hold(&hold(
            (&account, &request, &key),
            1,
            Some(snapshot(10, 0)),
            None,
            Some(cap),
            spent(u64::MAX, 0),
        ));
        assert!(matches!(
            sum,
            Err(WalletError::Rejected(BillingRejection::Overflow))
        ));
    }

    #[test]
    fn settle_splits_revenue_and_remainder_and_release_returns_the_hold() {
        let (account, request, key) = ids();
        let open = OpenHold {
            key: key.clone(),
            amount: money(40),
        };
        let settled = post_settle(&SettleRequest {
            account: &account,
            request: &request,
            amount: money(25),
            at: UnixMillis(11),
            balance: snapshot(60, 40),
            existing: Some(ExistingHold::Open(open.clone())),
        });
        let settled = must(settled);
        assert_eq!(settled.balance.available.minor_units, 75);
        assert_eq!(settled.balance.held.minor_units, 0);
        assert!(matches!(
            (
                &settled.entries[0].account,
                &settled.entries[1].account,
                settled.entries[0].amount.minor_units
            ),
            (LedgerAccount::Held(_), LedgerAccount::Revenue, 25)
        ));
        assert!(matches!(
            (
                &settled.entries[2].account,
                &settled.entries[3].account,
                settled.entries[3].amount.minor_units
            ),
            (LedgerAccount::Held(_), LedgerAccount::Available(_), 15)
        ));
        let released = post_release(&ReleaseRequest {
            account: &account,
            request: &request,
            at: UnixMillis(12),
            balance: snapshot(60, 40),
            existing: Some(ExistingHold::Open(open)),
        });
        let released = must(released);
        assert_eq!(released.balance.available.minor_units, 100);
        assert_eq!(released.balance.held.minor_units, 0);
        assert!(matches!(
            (&released.entries[0].direction, &released.entries[1].account),
            (Direction::Debit, LedgerAccount::Available(_))
        ));
    }

    #[test]
    fn settle_and_release_reject_terminal_states_without_rewriting_the_amount() {
        let (account, request, key) = ids();
        let balance = snapshot(60, 40);
        let too_much = post_settle(&SettleRequest {
            account: &account,
            request: &request,
            amount: money(41),
            at: UnixMillis(1),
            balance,
            existing: Some(ExistingHold::Open(OpenHold {
                key: key.clone(),
                amount: money(40),
            })),
        });
        assert!(matches!(
            too_much,
            Err(WalletError::Rejected(
                BillingRejection::SettlementExceedsHold
            ))
        ));
        let foreign = post_settle(&SettleRequest {
            account: &account,
            request: &request,
            amount: cny(10),
            at: UnixMillis(1),
            balance,
            existing: Some(ExistingHold::Open(OpenHold {
                key: key.clone(),
                amount: money(40),
            })),
        });
        assert!(matches!(
            foreign,
            Err(WalletError::Rejected(BillingRejection::CurrencyMismatch))
        ));
        for (existing, reason) in [
            (None, BillingRejection::HoldNotFound),
            (
                Some(ExistingHold::Settled),
                BillingRejection::AlreadySettled,
            ),
            (
                Some(ExistingHold::Released),
                BillingRejection::AlreadyReleased,
            ),
        ] {
            let settled = post_settle(&SettleRequest {
                account: &account,
                request: &request,
                amount: money(1),
                at: UnixMillis(1),
                balance,
                existing: existing.clone(),
            });
            let released = post_release(&ReleaseRequest {
                account: &account,
                request: &request,
                at: UnixMillis(1),
                balance,
                existing,
            });
            assert!(matches!(settled, Err(WalletError::Rejected(got)) if got == reason));
            assert!(matches!(released, Err(WalletError::Rejected(got)) if got == reason));
        }
        let short = post_release(&ReleaseRequest {
            account: &account,
            request: &request,
            at: UnixMillis(1),
            balance: snapshot(60, 39),
            existing: Some(ExistingHold::Open(OpenHold {
                key,
                amount: money(40),
            })),
        });
        assert!(matches!(
            short,
            Err(WalletError::Storage(StorageFault::HeldBelowOpen))
        ));
    }

    #[test]
    fn a_zero_amount_writes_no_entry() {
        let (account, request, key) = ids();
        let posted = must(post_hold(&hold(
            (&account, &request, &key),
            0,
            Some(snapshot(5, 1)),
            None,
            None,
            spent(0, 0),
        )));
        assert_eq!(posted.entries.len(), 0);
        assert_eq!(posted.balance.available.minor_units, 5);
        assert_eq!(posted.balance.held.minor_units, 1);
    }
}

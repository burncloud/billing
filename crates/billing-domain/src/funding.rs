use billing::model::{Entry, EntryRef, LedgerAccount};
use kernel::model::{AccountId, CreditId, Currency, KeyId, Money, RequestId, UnixMillis};

use crate::posting::push_pair;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Opened {
    Created,
    AlreadyOpen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Credited {
    Applied,
    AlreadyApplied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FundingRejection {
    UnsupportedCurrency,
    EmptyCreditId,
    ZeroAmount,
    NotOpened,
    ConflictingCredit,
    Overflow,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FundingError<E> {
    Rejected(FundingRejection),
    Storage(E),
}

/// 首期只给 USD 开户和入账。
///
/// # Errors
///
/// 不是 USD 时返回 `FundingRejection::UnsupportedCurrency`。
pub fn check_funding_currency(currency: Currency) -> Result<(), FundingRejection> {
    match currency {
        Currency::Usd => Ok(()),
        Currency::Cny => Err(FundingRejection::UnsupportedCurrency),
    }
}

/// 一个币种的余额行里和入账有关的两列。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FundingRow {
    pub available: u64,
    pub deposited: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct CreditRequest<'a> {
    pub account: &'a AccountId,
    pub credit: &'a CreditId,
    pub amount: Money,
    pub row: Option<FundingRow>,
    pub existing: Option<Money>,
    pub at: UnixMillis,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditApplied {
    pub row: FundingRow,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreditPosting {
    Apply(CreditApplied),
    AlreadyApplied,
}

/// `request.row` 是 `request.amount.currency` 的余额行，`request.existing` 是同一账户下这个单号已经记过的金额。
/// 入账同时加 `available` 和 `deposited`，并写两条金额相等的分录：借 `LedgerAccount::Funding`、贷
/// `LedgerAccount::Available(account)`，`reference` 都是 `EntryRef::Credit(credit)`，`key` 为 `None`，
/// 时间是 `request.at`。重复入账返回 `CreditPosting::AlreadyApplied`，不产生分录。
///
/// # Errors
///
/// 单号为空时返回 `FundingRejection::EmptyCreditId`。
/// 金额为 0 时返回 `FundingRejection::ZeroAmount`。
/// 币种不是 USD 时返回 `FundingRejection::UnsupportedCurrency`。
/// 单号已记过、但金额不同时返回 `FundingRejection::ConflictingCredit`。
/// 没有余额行时返回 `FundingRejection::NotOpened`。
/// 相加溢出时返回 `FundingRejection::Overflow`。
pub fn post_credit(request: &CreditRequest<'_>) -> Result<CreditPosting, FundingRejection> {
    if request.credit.0.is_empty() {
        return Err(FundingRejection::EmptyCreditId);
    }
    if request.amount.minor_units == 0 {
        return Err(FundingRejection::ZeroAmount);
    }
    check_funding_currency(request.amount.currency)?;
    match request.existing {
        Some(previous) if previous == request.amount => return Ok(CreditPosting::AlreadyApplied),
        Some(_) => return Err(FundingRejection::ConflictingCredit),
        None => {}
    }
    let row = request.row.ok_or(FundingRejection::NotOpened)?;
    let available = row
        .available
        .checked_add(request.amount.minor_units)
        .ok_or(FundingRejection::Overflow)?;
    let deposited = row
        .deposited
        .checked_add(request.amount.minor_units)
        .ok_or(FundingRejection::Overflow)?;
    Ok(CreditPosting::Apply(CreditApplied {
        row: FundingRow {
            available,
            deposited,
        },
        entries: credit_entries(request.account, request.credit, request.amount, request.at),
    }))
}

/// 一笔入账的两条分录，不检查币种和金额：借 `LedgerAccount::Funding`、贷 `LedgerAccount::Available(account)`。
/// 金额为 0 时返回空列表。
#[must_use]
pub fn credit_entries(
    account: &AccountId,
    credit: &CreditId,
    amount: Money,
    at: UnixMillis,
) -> Vec<Entry> {
    let mut entries = Vec::new();
    push_pair(
        &mut entries,
        &EntryRef::Credit(credit.clone()),
        None,
        amount,
        at,
        LedgerAccount::Funding,
        LedgerAccount::Available(account.clone()),
    );
    entries
}

/// 还没结算或释放的冻结。`created_at` 是冻结写入时钱包时钟的时刻。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PendingHold {
    pub account: AccountId,
    pub request: RequestId,
    pub key: KeyId,
    pub amount: Money,
    pub created_at: UnixMillis,
}

#[cfg(test)]
mod tests {
    use billing::model::{Direction, Entry, EntryRef, LedgerAccount};
    use kernel::model::{AccountId, CreditId, Currency, Money, UnixMillis};

    use super::{
        check_funding_currency, post_credit, CreditApplied, CreditPosting, CreditRequest,
        FundingRejection, FundingRow,
    };

    fn usd(minor_units: u64) -> Money {
        Money {
            currency: Currency::Usd,
            minor_units,
        }
    }

    fn id(value: &str) -> CreditId {
        CreditId(String::from(value))
    }

    fn row(available: u64, deposited: u64) -> FundingRow {
        FundingRow {
            available,
            deposited,
        }
    }

    fn account() -> AccountId {
        AccountId(String::from("acct"))
    }

    fn post(
        credit: &CreditId,
        amount: Money,
        row: Option<FundingRow>,
        existing: Option<Money>,
    ) -> Result<CreditPosting, FundingRejection> {
        post_credit(&CreditRequest {
            account: &account(),
            credit,
            amount,
            row,
            existing,
            at: UnixMillis(7),
        })
    }

    #[test]
    fn a_new_credit_raises_available_and_deposited_together() {
        let posting = post(&id("c-1"), usd(40), Some(row(10, 25)), None);
        let Ok(CreditPosting::Apply(CreditApplied { row, .. })) = posting else {
            panic!("{posting:?}");
        };
        assert_eq!(
            row,
            FundingRow {
                available: 50,
                deposited: 65,
            }
        );
    }

    #[test]
    fn a_new_credit_debits_funding_and_credits_available() {
        let posting = post(&id("c-1"), usd(40), Some(row(10, 25)), None);
        let Ok(CreditPosting::Apply(applied)) = posting else {
            panic!("{posting:?}");
        };
        let expected = |account, direction| Entry {
            reference: EntryRef::Credit(id("c-1")),
            account,
            key: None,
            direction,
            amount: usd(40),
            at: UnixMillis(7),
        };
        assert_eq!(
            applied.entries,
            [
                expected(LedgerAccount::Funding, Direction::Debit),
                expected(LedgerAccount::Available(account()), Direction::Credit),
            ]
        );
    }

    #[test]
    fn a_repeated_credit_is_applied_once_and_writes_nothing() {
        assert_eq!(
            post(&id("c-1"), usd(40), Some(row(50, 65)), Some(usd(40))),
            Ok(CreditPosting::AlreadyApplied)
        );
    }

    #[test]
    fn a_reused_id_with_another_amount_conflicts() {
        assert_eq!(
            post(&id("c-1"), usd(41), Some(row(50, 65)), Some(usd(40))),
            Err(FundingRejection::ConflictingCredit)
        );
    }

    #[test]
    fn only_usd_can_be_funded() {
        let cny = Money {
            currency: Currency::Cny,
            minor_units: 40,
        };
        assert_eq!(check_funding_currency(Currency::Usd), Ok(()));
        assert_eq!(
            check_funding_currency(Currency::Cny),
            Err(FundingRejection::UnsupportedCurrency)
        );
        assert_eq!(
            post(&id("c-1"), cny, Some(row(50, 65)), None),
            Err(FundingRejection::UnsupportedCurrency)
        );
        assert_eq!(
            post(&id("c-1"), cny, Some(row(50, 65)), Some(cny)),
            Err(FundingRejection::UnsupportedCurrency)
        );
    }

    #[test]
    fn invalid_credits_are_rejected_before_touching_the_row() {
        assert_eq!(
            post(&id(""), usd(1), Some(row(0, 0)), None),
            Err(FundingRejection::EmptyCreditId)
        );
        assert_eq!(
            post(&id("c-0"), usd(0), Some(row(0, 0)), None),
            Err(FundingRejection::ZeroAmount)
        );
        assert_eq!(
            post(&id("c-2"), usd(1), None, None),
            Err(FundingRejection::NotOpened)
        );
    }

    #[test]
    fn overflow_on_either_column_is_rejected() {
        assert_eq!(
            post(&id("c-3"), usd(2), Some(row(u64::MAX - 1, 0)), None),
            Err(FundingRejection::Overflow)
        );
        assert_eq!(
            post(&id("c-3"), usd(2), Some(row(0, u64::MAX - 1)), None),
            Err(FundingRejection::Overflow)
        );
        let posting = post(
            &id("c-3"),
            usd(1),
            Some(row(u64::MAX - 1, u64::MAX - 1)),
            None,
        );
        let Ok(CreditPosting::Apply(applied)) = posting else {
            panic!("{posting:?}");
        };
        assert_eq!(
            applied.row,
            FundingRow {
                available: u64::MAX,
                deposited: u64::MAX,
            }
        );
        assert_eq!(applied.entries.len(), 2);
    }
}

use kernel::model::{AccountId, Currency, KeyId, Money, RequestId, UnixMillis};

/// 管理员入账的单号，在同一个账户内唯一。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CreditId(pub String);

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CreditPosting {
    Apply(FundingRow),
    AlreadyApplied,
}

/// `row` 是 `amount.currency` 的余额行，`existing` 是同一账户下这个单号已经记过的金额。
/// 入账同时加 `available` 和 `deposited`，两边相等，`check_ledger` 的守恒式保持成立。
///
/// # Errors
///
/// 单号为空时返回 `FundingRejection::EmptyCreditId`。
/// 金额为 0 时返回 `FundingRejection::ZeroAmount`。
/// 币种不是 USD 时返回 `FundingRejection::UnsupportedCurrency`。
/// 单号已记过、但金额不同时返回 `FundingRejection::ConflictingCredit`。
/// 没有余额行时返回 `FundingRejection::NotOpened`。
/// 相加溢出时返回 `FundingRejection::Overflow`。
pub fn post_credit(
    credit: &CreditId,
    amount: Money,
    row: Option<FundingRow>,
    existing: Option<Money>,
) -> Result<CreditPosting, FundingRejection> {
    if credit.0.is_empty() {
        return Err(FundingRejection::EmptyCreditId);
    }
    if amount.minor_units == 0 {
        return Err(FundingRejection::ZeroAmount);
    }
    check_funding_currency(amount.currency)?;
    match existing {
        Some(previous) if previous == amount => return Ok(CreditPosting::AlreadyApplied),
        Some(_) => return Err(FundingRejection::ConflictingCredit),
        None => {}
    }
    let row = row.ok_or(FundingRejection::NotOpened)?;
    let available = row
        .available
        .checked_add(amount.minor_units)
        .ok_or(FundingRejection::Overflow)?;
    let deposited = row
        .deposited
        .checked_add(amount.minor_units)
        .ok_or(FundingRejection::Overflow)?;
    Ok(CreditPosting::Apply(FundingRow {
        available,
        deposited,
    }))
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
    use kernel::model::{Currency, Money};

    use super::{
        check_funding_currency, post_credit, CreditId, CreditPosting, FundingRejection, FundingRow,
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

    #[test]
    fn a_new_credit_raises_available_and_deposited_together() {
        assert_eq!(
            post_credit(&id("c-1"), usd(40), Some(row(10, 25)), None),
            Ok(CreditPosting::Apply(FundingRow {
                available: 50,
                deposited: 65,
            }))
        );
    }

    #[test]
    fn a_repeated_credit_is_applied_once() {
        assert_eq!(
            post_credit(&id("c-1"), usd(40), Some(row(50, 65)), Some(usd(40))),
            Ok(CreditPosting::AlreadyApplied)
        );
    }

    #[test]
    fn a_reused_id_with_another_amount_conflicts() {
        assert_eq!(
            post_credit(&id("c-1"), usd(41), Some(row(50, 65)), Some(usd(40))),
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
            post_credit(&id("c-1"), cny, Some(row(50, 65)), None),
            Err(FundingRejection::UnsupportedCurrency)
        );
        assert_eq!(
            post_credit(&id("c-1"), cny, Some(row(50, 65)), Some(cny)),
            Err(FundingRejection::UnsupportedCurrency)
        );
    }

    #[test]
    fn invalid_credits_are_rejected_before_touching_the_row() {
        assert_eq!(
            post_credit(&id(""), usd(1), Some(row(0, 0)), None),
            Err(FundingRejection::EmptyCreditId)
        );
        assert_eq!(
            post_credit(&id("c-0"), usd(0), Some(row(0, 0)), None),
            Err(FundingRejection::ZeroAmount)
        );
        assert_eq!(
            post_credit(&id("c-2"), usd(1), None, None),
            Err(FundingRejection::NotOpened)
        );
    }

    #[test]
    fn overflow_on_either_column_is_rejected() {
        assert_eq!(
            post_credit(&id("c-3"), usd(2), Some(row(u64::MAX - 1, 0)), None),
            Err(FundingRejection::Overflow)
        );
        assert_eq!(
            post_credit(&id("c-3"), usd(2), Some(row(0, u64::MAX - 1)), None),
            Err(FundingRejection::Overflow)
        );
        assert_eq!(
            post_credit(
                &id("c-3"),
                usd(1),
                Some(row(u64::MAX - 1, u64::MAX - 1)),
                None
            ),
            Ok(CreditPosting::Apply(FundingRow {
                available: u64::MAX,
                deposited: u64::MAX,
            }))
        );
    }
}

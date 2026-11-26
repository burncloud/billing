use billing::model::{Direction, Entry, LedgerAccount};
use kernel::model::{AccountId, Currency, RequestId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurrencyFunds {
    pub currency: Currency,
    pub available: u64,
    pub held: u64,
    pub deposits: u64,
    pub revenue: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvariantViolation {
    Unbalanced {
        currency: Currency,
        debits: u64,
        credits: u64,
    },
    NotConserved {
        currency: Currency,
    },
    Overflow {
        currency: Currency,
    },
    RepeatedCurrency {
        currency: Currency,
    },
}

/// `LedgerAccount::Revenue` 不带币种。`revenue` 是该币种自己的收入账户。
/// 每个币种在 `funds` 里只出现一次。
///
/// # Errors
///
/// 同一币种的借方合计不等于贷方合计时返回 `InvariantViolation::Unbalanced`。
/// `available + held` 不等于 `deposits - revenue` 时返回 `InvariantViolation::NotConserved`。
/// 合计溢出时返回 `InvariantViolation::Overflow`。
/// 同一币种出现两行 `CurrencyFunds` 时返回 `InvariantViolation::RepeatedCurrency`。
pub fn check_ledger(entries: &[Entry], funds: &[CurrencyFunds]) -> Result<(), InvariantViolation> {
    for currency in [Currency::Usd, Currency::Cny] {
        let mut debits = 0_u64;
        let mut credits = 0_u64;
        for entry in entries {
            if entry.amount.currency != currency {
                continue;
            }
            match entry.direction {
                Direction::Debit => {
                    debits = debits
                        .checked_add(entry.amount.minor_units)
                        .ok_or(InvariantViolation::Overflow { currency })?;
                }
                Direction::Credit => {
                    credits = credits
                        .checked_add(entry.amount.minor_units)
                        .ok_or(InvariantViolation::Overflow { currency })?;
                }
            }
        }
        if debits != credits {
            return Err(InvariantViolation::Unbalanced {
                currency,
                debits,
                credits,
            });
        }
    }
    let mut seen = [false, false];
    for row in funds {
        let index = currency_index(row.currency);
        if seen[index] {
            return Err(InvariantViolation::RepeatedCurrency {
                currency: row.currency,
            });
        }
        seen[index] = true;
        let left = row
            .available
            .checked_add(row.held)
            .ok_or(InvariantViolation::Overflow {
                currency: row.currency,
            })?;
        let Some(right) = row.deposits.checked_sub(row.revenue) else {
            return Err(InvariantViolation::NotConserved {
                currency: row.currency,
            });
        };
        if left != right {
            return Err(InvariantViolation::NotConserved {
                currency: row.currency,
            });
        }
    }
    Ok(())
}

/// 只统计该 `request` 在 `Held(account)` 上的分录。
///
/// # Errors
///
/// 任一币种的借方合计不等于贷方合计时返回 `InvariantViolation::Unbalanced`。
/// 合计溢出时返回 `InvariantViolation::Overflow`。
pub fn request_held_flat(
    entries: &[Entry],
    account: &AccountId,
    request: &RequestId,
) -> Result<(), InvariantViolation> {
    for currency in [Currency::Usd, Currency::Cny] {
        let mut debits = 0_u64;
        let mut credits = 0_u64;
        for entry in entries {
            if entry.request != *request || entry.amount.currency != currency {
                continue;
            }
            let LedgerAccount::Held(holder) = &entry.account else {
                continue;
            };
            if holder != account {
                continue;
            }
            match entry.direction {
                Direction::Debit => {
                    debits = debits
                        .checked_add(entry.amount.minor_units)
                        .ok_or(InvariantViolation::Overflow { currency })?;
                }
                Direction::Credit => {
                    credits = credits
                        .checked_add(entry.amount.minor_units)
                        .ok_or(InvariantViolation::Overflow { currency })?;
                }
            }
        }
        if debits != credits {
            return Err(InvariantViolation::Unbalanced {
                currency,
                debits,
                credits,
            });
        }
    }
    Ok(())
}

fn currency_index(currency: Currency) -> usize {
    match currency {
        Currency::Usd => 0,
        Currency::Cny => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::{check_ledger, request_held_flat, CurrencyFunds, InvariantViolation};

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
    use crate::posting::{
        post_hold, post_release, post_settle, ExistingHold, HoldRequest, OpenHold, ReleaseRequest,
        SettleRequest, SpendSoFar, WalletSnapshot,
    };
    use billing::model::Entry;
    use kernel::model::{AccountId, Currency, KeyId, Money, RequestId, UnixMillis};

    fn money(currency: Currency, minor_units: u64) -> Money {
        Money {
            currency,
            minor_units,
        }
    }

    fn funds(
        currency: Currency,
        available: u64,
        held: u64,
        deposits: u64,
        revenue: u64,
    ) -> CurrencyFunds {
        CurrencyFunds {
            currency,
            available,
            held,
            deposits,
            revenue,
        }
    }

    #[test]
    fn each_currency_balances_and_conserves_on_its_own() {
        let account = AccountId(String::from("acct"));
        let request = RequestId(String::from("req"));
        let key = KeyId(String::from("key"));
        let held = post_hold(&HoldRequest {
            account: &account,
            request: &request,
            key: &key,
            cap: None,
            amount: money(Currency::Usd, 40),
            at: UnixMillis(1),
            balance: Some(WalletSnapshot {
                available: money(Currency::Usd, 100),
                held: money(Currency::Usd, 0),
            }),
            existing: None,
            spent: SpendSoFar {
                settled: money(Currency::Usd, 0),
                open: money(Currency::Usd, 0),
            },
        });
        let held = must(held);
        assert!(check_ledger(&held.entries, &[funds(Currency::Usd, 60, 40, 100, 0)]).is_ok());
        assert!(request_held_flat(&held.entries, &account, &request).is_err());
        let settled = post_settle(&SettleRequest {
            account: &account,
            request: &request,
            amount: money(Currency::Usd, 25),
            at: UnixMillis(2),
            balance: held.balance,
            existing: Some(ExistingHold::Open(OpenHold {
                key: key.clone(),
                amount: money(Currency::Usd, 40),
            })),
        });
        let settled = must(settled);
        let mut entries = held.entries;
        entries.extend(settled.entries);
        assert!(request_held_flat(&entries, &account, &request).is_ok());
        assert!(check_ledger(&entries, &[funds(Currency::Usd, 75, 0, 100, 25)]).is_ok());
        let cny_only = funds(Currency::Cny, 8, 2, 10, 0);
        assert!(check_ledger(&entries, &[funds(Currency::Usd, 75, 0, 100, 25), cny_only]).is_ok());
    }

    #[test]
    fn release_flattens_held_and_rejections_name_the_break() {
        let account = AccountId(String::from("acct"));
        let request = RequestId(String::from("req"));
        let key = KeyId(String::from("key"));
        let held = post_hold(&HoldRequest {
            account: &account,
            request: &request,
            key: &key,
            cap: None,
            amount: money(Currency::Usd, 40),
            at: UnixMillis(1),
            balance: Some(WalletSnapshot {
                available: money(Currency::Usd, 100),
                held: money(Currency::Usd, 0),
            }),
            existing: None,
            spent: SpendSoFar {
                settled: money(Currency::Usd, 0),
                open: money(Currency::Usd, 0),
            },
        });
        let held = must(held);
        let released = post_release(&ReleaseRequest {
            account: &account,
            request: &request,
            at: UnixMillis(3),
            balance: held.balance,
            existing: Some(ExistingHold::Open(OpenHold {
                key,
                amount: money(Currency::Usd, 40),
            })),
        });
        let released = must(released);
        let mut entries = held.entries;
        entries.extend(released.entries);
        assert!(request_held_flat(&entries, &account, &request).is_ok());
        assert!(check_ledger(&entries, &[funds(Currency::Usd, 100, 0, 100, 0)]).is_ok());
        assert!(matches!(
            check_ledger(&entries, &[funds(Currency::Usd, 90, 0, 100, 0)]),
            Err(InvariantViolation::NotConserved {
                currency: Currency::Usd
            })
        ));
        let mut broken: Vec<Entry> = entries;
        broken.pop();
        assert!(matches!(
            check_ledger(&broken, &[]),
            Err(InvariantViolation::Unbalanced {
                currency: Currency::Usd,
                ..
            })
        ));
        assert!(matches!(
            check_ledger(
                &[],
                &[
                    funds(Currency::Usd, 1, 0, 1, 0),
                    funds(Currency::Usd, 1, 0, 1, 0)
                ]
            ),
            Err(InvariantViolation::RepeatedCurrency {
                currency: Currency::Usd
            })
        ));
    }
}

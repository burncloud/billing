use std::collections::{HashMap, HashSet};

use billing::model::{Direction, Entry, EntryRef, LedgerAccount};
use kernel::model::{AccountId, Currency, RequestId};

/// 一个币种的钱包表合计。充值总额不在这里，由 `deposits_in` 从 `LedgerAccount::Funding` 的借方推导。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurrencyFunds {
    pub currency: Currency,
    pub available: u64,
    pub held: u64,
    pub revenue: u64,
}

/// 钱包表里一个账户在一个币种上的一行：可用余额和冻结额。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountFunds {
    pub account: AccountId,
    pub currency: Currency,
    pub available: u64,
    pub held: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    RepeatedAccount {
        account: AccountId,
        currency: Currency,
    },
    AvailableMismatch {
        account: AccountId,
        currency: Currency,
    },
    HeldMismatch {
        account: AccountId,
        currency: Currency,
    },
}

/// 充值总额：该币种下 `LedgerAccount::Funding` 借方分录的合计。
///
/// # Errors
///
/// 合计溢出时返回 `InvariantViolation::Overflow`。
pub fn deposits_in(entries: &[Entry], currency: Currency) -> Result<u64, InvariantViolation> {
    let mut deposits = 0_u64;
    for entry in entries {
        if entry.amount.currency != currency
            || entry.account != LedgerAccount::Funding
            || entry.direction != Direction::Debit
        {
            continue;
        }
        deposits = deposits
            .checked_add(entry.amount.minor_units)
            .ok_or(InvariantViolation::Overflow { currency })?;
    }
    Ok(deposits)
}

/// `LedgerAccount::Revenue` 和 `LedgerAccount::Funding` 不带币种。`funds` 里的 `revenue` 是该币种自己的收入账户，
/// 充值总额从 `Funding` 的借方推导。每个币种在 `funds` 里只出现一次，每个账户币种在 `accounts` 里只出现一次。
/// 分录里出现、但 `accounts` 里没有行的账户，按余额和冻结额都为 0 核对。
///
/// # Errors
///
/// 同一币种的借方合计不等于贷方合计时返回 `InvariantViolation::Unbalanced`。
/// `available + held` 不等于 `Funding` 借方合计减 `revenue` 时返回 `InvariantViolation::NotConserved`。
/// 合计溢出时返回 `InvariantViolation::Overflow`。
/// 同一币种出现两行 `CurrencyFunds` 时返回 `InvariantViolation::RepeatedCurrency`。
/// 同一账户币种出现两行 `AccountFunds` 时返回 `InvariantViolation::RepeatedAccount`。
/// 账户 `Available` 分录的净额（贷减借）不等于该账户的 `available` 时返回
/// `InvariantViolation::AvailableMismatch`，`Held` 分录的净额不等于 `held` 时返回
/// `InvariantViolation::HeldMismatch`。
pub fn check_ledger(
    entries: &[Entry],
    funds: &[CurrencyFunds],
    accounts: &[AccountFunds],
) -> Result<(), InvariantViolation> {
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
        let Some(right) = deposits_in(entries, row.currency)?.checked_sub(row.revenue) else {
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
    check_accounts(entries, accounts)
}

type AccountKey = (AccountId, Currency);

fn check_accounts(entries: &[Entry], accounts: &[AccountFunds]) -> Result<(), InvariantViolation> {
    let mut nets: HashMap<AccountKey, (i128, i128)> = HashMap::new();
    let mut order: Vec<AccountKey> = Vec::new();
    for entry in entries {
        let (account, available_side) = match &entry.account {
            LedgerAccount::Available(account) => (account, true),
            LedgerAccount::Held(account) => (account, false),
            LedgerAccount::Revenue | LedgerAccount::Funding => continue,
        };
        let key = (account.clone(), entry.amount.currency);
        if !nets.contains_key(&key) {
            order.push(key.clone());
        }
        let net = nets.entry(key).or_insert((0, 0));
        let amount = i128::from(entry.amount.minor_units);
        let signed = match entry.direction {
            Direction::Credit => amount,
            Direction::Debit => -amount,
        };
        let slot = if available_side {
            &mut net.0
        } else {
            &mut net.1
        };
        *slot = slot
            .checked_add(signed)
            .ok_or(InvariantViolation::Overflow {
                currency: entry.amount.currency,
            })?;
    }
    let mut listed: HashSet<AccountKey> = HashSet::new();
    for row in accounts {
        let key = (row.account.clone(), row.currency);
        if !listed.insert(key.clone()) {
            return Err(InvariantViolation::RepeatedAccount {
                account: row.account.clone(),
                currency: row.currency,
            });
        }
        let (available, held) = nets.get(&key).copied().unwrap_or((0, 0));
        if available != i128::from(row.available) {
            return Err(InvariantViolation::AvailableMismatch {
                account: row.account.clone(),
                currency: row.currency,
            });
        }
        if held != i128::from(row.held) {
            return Err(InvariantViolation::HeldMismatch {
                account: row.account.clone(),
                currency: row.currency,
            });
        }
    }
    for key in order {
        if listed.contains(&key) {
            continue;
        }
        let (available, held) = nets.get(&key).copied().unwrap_or((0, 0));
        let (account, currency) = key;
        if available != 0 {
            return Err(InvariantViolation::AvailableMismatch { account, currency });
        }
        if held != 0 {
            return Err(InvariantViolation::HeldMismatch { account, currency });
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
            if entry.amount.currency != currency {
                continue;
            }
            match &entry.reference {
                EntryRef::Request(id) if id == request => {}
                EntryRef::Request(_) | EntryRef::Credit(_) => continue,
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
    use super::{
        check_ledger, deposits_in, request_held_flat, AccountFunds, CurrencyFunds,
        InvariantViolation,
    };
    use crate::posting::{
        post_hold, post_release, post_settle, ExistingHold, HoldRequest, OpenHold, ReleaseRequest,
        SettleRequest, SpendSoFar, WalletSnapshot,
    };
    use billing::model::{Direction, Entry, EntryRef, LedgerAccount};
    use kernel::model::{AccountId, CreditId, Currency, KeyId, Money, RequestId, UnixMillis};

    fn must<T, E: core::fmt::Debug>(result: Result<T, E>) -> T {
        match result {
            Ok(value) => value,
            Err(error) => panic!("{error:?}"),
        }
    }

    fn money(currency: Currency, minor_units: u64) -> Money {
        Money {
            currency,
            minor_units,
        }
    }

    fn funds(currency: Currency, available: u64, held: u64, revenue: u64) -> CurrencyFunds {
        CurrencyFunds {
            currency,
            available,
            held,
            revenue,
        }
    }

    fn row(account: &AccountId, currency: Currency, available: u64, held: u64) -> AccountFunds {
        AccountFunds {
            account: account.clone(),
            currency,
            available,
            held,
        }
    }

    fn funded(account: &AccountId, id: &str, amount: Money) -> Vec<Entry> {
        let reference = EntryRef::Credit(CreditId(String::from(id)));
        vec![
            Entry {
                reference: reference.clone(),
                account: LedgerAccount::Funding,
                key: None,
                direction: Direction::Debit,
                amount,
                at: UnixMillis(0),
            },
            Entry {
                reference,
                account: LedgerAccount::Available(account.clone()),
                key: None,
                direction: Direction::Credit,
                amount,
                at: UnixMillis(0),
            },
        ]
    }

    fn open_hold(
        account: &AccountId,
        request: &RequestId,
        key: &KeyId,
        amount: u64,
    ) -> crate::posting::Posted {
        must(post_hold(&HoldRequest {
            account,
            request,
            key,
            cap: None,
            amount: money(Currency::Usd, amount),
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
        }))
    }

    fn ids() -> (AccountId, RequestId, KeyId) {
        (
            AccountId(String::from("acct")),
            RequestId(String::from("req")),
            KeyId(String::from("key")),
        )
    }

    #[test]
    fn each_currency_balances_and_conserves_on_its_own() {
        let (account, request, key) = ids();
        let held = open_hold(&account, &request, &key, 40);
        let mut opened = funded(&account, "c-1", money(Currency::Usd, 100));
        opened.extend(held.entries.clone());
        assert!(check_ledger(
            &opened,
            &[funds(Currency::Usd, 60, 40, 0)],
            &[row(&account, Currency::Usd, 60, 40)]
        )
        .is_ok());
        assert!(request_held_flat(&opened, &account, &request).is_err());
        let settled = must(post_settle(&SettleRequest {
            account: &account,
            request: &request,
            amount: money(Currency::Usd, 25),
            at: UnixMillis(2),
            balance: held.balance,
            existing: Some(ExistingHold::Open(OpenHold {
                key: key.clone(),
                amount: money(Currency::Usd, 40),
            })),
        }));
        let mut entries = opened;
        entries.extend(settled.entries);
        assert!(request_held_flat(&entries, &account, &request).is_ok());
        let usd_row = row(&account, Currency::Usd, 75, 0);
        assert!(check_ledger(
            &entries,
            &[funds(Currency::Usd, 75, 0, 25)],
            std::slice::from_ref(&usd_row)
        )
        .is_ok());
        let other = AccountId(String::from("cny-acct"));
        entries.extend(funded(&other, "c-cny", money(Currency::Cny, 10)));
        assert!(check_ledger(
            &entries,
            &[
                funds(Currency::Usd, 75, 0, 25),
                funds(Currency::Cny, 10, 0, 0)
            ],
            &[usd_row, row(&other, Currency::Cny, 10, 0)]
        )
        .is_ok());
    }

    #[test]
    fn release_flattens_held_and_rejections_name_the_break() {
        let (account, request, key) = ids();
        let held = open_hold(&account, &request, &key, 40);
        let released = must(post_release(&ReleaseRequest {
            account: &account,
            request: &request,
            at: UnixMillis(3),
            balance: held.balance,
            existing: Some(ExistingHold::Open(OpenHold {
                key,
                amount: money(Currency::Usd, 40),
            })),
        }));
        let mut entries = funded(&account, "c-1", money(Currency::Usd, 100));
        entries.extend(held.entries);
        entries.extend(released.entries);
        let balances = [row(&account, Currency::Usd, 100, 0)];
        assert!(request_held_flat(&entries, &account, &request).is_ok());
        assert!(check_ledger(&entries, &[funds(Currency::Usd, 100, 0, 0)], &balances).is_ok());
        assert!(matches!(
            check_ledger(&entries, &[funds(Currency::Usd, 90, 0, 0)], &balances),
            Err(InvariantViolation::NotConserved {
                currency: Currency::Usd
            })
        ));
        let mut broken: Vec<Entry> = entries;
        broken.pop();
        assert!(matches!(
            check_ledger(&broken, &[], &[]),
            Err(InvariantViolation::Unbalanced {
                currency: Currency::Usd,
                ..
            })
        ));
        assert!(matches!(
            check_ledger(
                &[],
                &[funds(Currency::Usd, 0, 0, 0), funds(Currency::Usd, 0, 0, 0)],
                &[]
            ),
            Err(InvariantViolation::RepeatedCurrency {
                currency: Currency::Usd
            })
        ));
    }

    #[test]
    fn deposits_come_from_funding_debits_only() {
        let account = AccountId(String::from("acct"));
        let mut entries = funded(&account, "c-1", money(Currency::Usd, 100));
        entries.extend(funded(&account, "c-2", money(Currency::Usd, 5)));
        entries.extend(funded(&account, "c-3", money(Currency::Cny, 9)));
        assert_eq!(deposits_in(&entries, Currency::Usd), Ok(105));
        assert_eq!(deposits_in(&entries, Currency::Cny), Ok(9));
        assert_eq!(deposits_in(&[], Currency::Usd), Ok(0));
        let mut huge = funded(&account, "c-big", money(Currency::Usd, u64::MAX));
        huge.extend(funded(&account, "c-more", money(Currency::Usd, 1)));
        assert_eq!(
            deposits_in(&huge, Currency::Usd),
            Err(InvariantViolation::Overflow {
                currency: Currency::Usd
            })
        );
    }

    #[test]
    fn a_balance_without_a_funding_entry_is_not_conserved() {
        let account = AccountId(String::from("acct"));
        let entries = funded(&account, "c-1", money(Currency::Usd, 100));
        let balances = [row(&account, Currency::Usd, 100, 0)];
        assert!(check_ledger(&entries, &[funds(Currency::Usd, 100, 0, 0)], &balances).is_ok());
        assert_eq!(
            check_ledger(&[], &[funds(Currency::Usd, 100, 0, 0)], &balances),
            Err(InvariantViolation::NotConserved {
                currency: Currency::Usd
            })
        );
        let mut inflated = entries;
        inflated.extend(funded(&account, "c-2", money(Currency::Usd, 1)));
        assert_eq!(
            check_ledger(&inflated, &[funds(Currency::Usd, 100, 0, 0)], &balances),
            Err(InvariantViolation::NotConserved {
                currency: Currency::Usd
            })
        );
    }

    #[test]
    fn each_account_must_match_its_available_and_held_entries() {
        let (account, request, key) = ids();
        let other = AccountId(String::from("other"));
        let held = open_hold(&account, &request, &key, 40);
        let mut entries = funded(&account, "c-1", money(Currency::Usd, 100));
        entries.extend(held.entries);
        entries.extend(funded(&other, "c-2", money(Currency::Usd, 30)));
        let total = funds(Currency::Usd, 90, 40, 0);
        let good = [
            row(&account, Currency::Usd, 60, 40),
            row(&other, Currency::Usd, 30, 0),
        ];
        assert!(check_ledger(&entries, &[total], &good).is_ok());
        let moved = [
            row(&account, Currency::Usd, 70, 40),
            row(&other, Currency::Usd, 20, 0),
        ];
        assert_eq!(
            check_ledger(&entries, &[total], &moved),
            Err(InvariantViolation::AvailableMismatch {
                account: account.clone(),
                currency: Currency::Usd
            })
        );
        let held_moved = [
            row(&account, Currency::Usd, 60, 30),
            row(&other, Currency::Usd, 30, 10),
        ];
        assert_eq!(
            check_ledger(&entries, &[total], &held_moved),
            Err(InvariantViolation::HeldMismatch {
                account: account.clone(),
                currency: Currency::Usd
            })
        );
        let missing = [row(&account, Currency::Usd, 60, 40)];
        assert_eq!(
            check_ledger(&entries, &[total], &missing),
            Err(InvariantViolation::AvailableMismatch {
                account: other.clone(),
                currency: Currency::Usd
            })
        );
        let repeated = [
            row(&account, Currency::Usd, 60, 40),
            row(&account, Currency::Usd, 60, 40),
        ];
        assert_eq!(
            check_ledger(&entries, &[total], &repeated),
            Err(InvariantViolation::RepeatedAccount {
                account,
                currency: Currency::Usd
            })
        );
    }

    #[test]
    fn held_entries_without_a_wallet_row_are_a_mismatch() {
        let (account, request, key) = ids();
        let held = open_hold(&account, &request, &key, 40);
        let mut entries = funded(&account, "c-1", money(Currency::Usd, 100));
        entries.extend(held.entries);
        let only_available = [row(&account, Currency::Usd, 60, 0)];
        assert_eq!(
            check_ledger(
                &entries,
                &[funds(Currency::Usd, 60, 40, 0)],
                &only_available
            ),
            Err(InvariantViolation::HeldMismatch {
                account: account.clone(),
                currency: Currency::Usd
            })
        );
        let ghost = AccountId(String::from("ghost"));
        let mut stray = vec![Entry {
            reference: EntryRef::Request(request),
            account: LedgerAccount::Held(ghost.clone()),
            key: None,
            direction: Direction::Credit,
            amount: money(Currency::Usd, 3),
            at: UnixMillis(1),
        }];
        stray.push(Entry {
            reference: EntryRef::Credit(CreditId(String::from("x"))),
            account: LedgerAccount::Funding,
            key: None,
            direction: Direction::Debit,
            amount: money(Currency::Usd, 3),
            at: UnixMillis(1),
        });
        assert_eq!(
            check_ledger(&stray, &[funds(Currency::Usd, 0, 3, 0)], &[]),
            Err(InvariantViolation::HeldMismatch {
                account: ghost,
                currency: Currency::Usd
            })
        );
    }

    fn held(
        account: &AccountId,
        reference: EntryRef,
        direction: Direction,
        amount: Money,
    ) -> Entry {
        Entry {
            reference,
            account: LedgerAccount::Held(account.clone()),
            key: None,
            direction,
            amount,
            at: UnixMillis(1),
        }
    }

    #[test]
    fn request_held_flat_reads_one_request_and_one_currency_at_a_time() {
        let account = AccountId(String::from("acct"));
        let first = RequestId(String::from("a"));
        let second = RequestId(String::from("b"));
        let by = |id: &RequestId| EntryRef::Request(id.clone());
        let entries = vec![
            held(
                &account,
                by(&first),
                Direction::Credit,
                money(Currency::Usd, 10),
            ),
            held(
                &account,
                by(&first),
                Direction::Debit,
                money(Currency::Usd, 10),
            ),
            held(
                &account,
                by(&first),
                Direction::Credit,
                money(Currency::Cny, 3),
            ),
            held(
                &account,
                by(&second),
                Direction::Debit,
                money(Currency::Cny, 3),
            ),
            held(
                &account,
                by(&second),
                Direction::Credit,
                money(Currency::Usd, 5),
            ),
            held(
                &account,
                EntryRef::Credit(CreditId(String::from("a"))),
                Direction::Credit,
                money(Currency::Usd, 9),
            ),
        ];
        assert_eq!(
            request_held_flat(&entries, &account, &first),
            Err(InvariantViolation::Unbalanced {
                currency: Currency::Cny,
                debits: 0,
                credits: 3,
            })
        );
        assert_eq!(
            request_held_flat(&entries, &account, &second),
            Err(InvariantViolation::Unbalanced {
                currency: Currency::Usd,
                debits: 0,
                credits: 5,
            })
        );
        assert_eq!(request_held_flat(&entries[..2], &account, &first), Ok(()));
    }
}

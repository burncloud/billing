use std::collections::HashMap;
use std::future::Future;
use std::sync::{Mutex, MutexGuard};

use billing::errors::{BillingRejection, WalletError};
use billing::model::{Direction, Entry, KeyCap, LedgerAccount};
use billing::ports::Wallet;
use billing_domain::{
    post_hold, post_release, post_settle, CurrencyFunds, ExistingHold, HoldRequest, OpenHold,
    ReleaseRequest, SettleRequest, SpendSoFar, StorageFault, WalletSnapshot,
};
use kernel::model::{AccountId, Currency, KeyId, Money, RequestId, UnixMillis};

use crate::contract::WalletContract;

type BalanceKey = (AccountId, Currency);
type HoldKey = (AccountId, RequestId);

#[derive(Debug)]
struct State {
    balances: HashMap<BalanceKey, WalletSnapshot>,
    deposits: HashMap<BalanceKey, u64>,
    holds: HashMap<HoldKey, ExistingHold>,
    entries: Vec<Entry>,
    now: UnixMillis,
}

/// 内存中的钱包。`hold` 不创建余额行。
#[derive(Debug)]
pub struct MemoryWallet {
    state: Mutex<State>,
}

impl MemoryWallet {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                balances: HashMap::new(),
                deposits: HashMap::new(),
                holds: HashMap::new(),
                entries: Vec::new(),
                now: UnixMillis(1),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl Default for MemoryWallet {
    fn default() -> Self {
        Self::new()
    }
}

impl WalletContract for MemoryWallet {
    fn deposit(&self, account: &AccountId, amount: Money) -> Result<(), WalletError<StorageFault>> {
        let mut state = self.lock();
        let key = (account.clone(), amount.currency);
        let (available, held) = match state.balances.get(&key) {
            Some(snapshot) => {
                let available = snapshot
                    .available
                    .minor_units
                    .checked_add(amount.minor_units)
                    .ok_or(WalletError::Rejected(BillingRejection::Overflow))?;
                (available, snapshot.held.minor_units)
            }
            None => (amount.minor_units, 0),
        };
        let already = match state.deposits.get(&key) {
            Some(value) => *value,
            None => 0,
        };
        let deposited = already
            .checked_add(amount.minor_units)
            .ok_or(WalletError::Rejected(BillingRejection::Overflow))?;
        state.deposits.insert(key.clone(), deposited);
        state.balances.insert(
            key,
            WalletSnapshot {
                available: Money {
                    currency: amount.currency,
                    minor_units: available,
                },
                held: Money {
                    currency: amount.currency,
                    minor_units: held,
                },
            },
        );
        Ok(())
    }

    fn hold(
        &self,
        account: &AccountId,
        request: &RequestId,
        key: &KeyId,
        cap: Option<KeyCap>,
        amount: Money,
    ) -> Result<(), WalletError<StorageFault>> {
        let mut state = self.lock();
        let balance_key = (account.clone(), amount.currency);
        let balance = state.balances.get(&balance_key).copied();
        let existing = state
            .holds
            .get(&(account.clone(), request.clone()))
            .cloned();
        let spent = match cap {
            Some(cap) => state.spend(key, amount.currency, cap.since)?,
            None => zero_spent(amount.currency),
        };
        let now = state.now;
        let posted = post_hold(&HoldRequest {
            account,
            request,
            key,
            cap,
            amount,
            at: now,
            balance,
            existing,
            spent,
        })?;
        state.balances.insert(balance_key, posted.balance);
        state.holds.insert(
            (account.clone(), request.clone()),
            ExistingHold::Open(OpenHold {
                key: key.clone(),
                amount,
            }),
        );
        state.entries.extend(posted.entries);
        Ok(())
    }

    fn settle(
        &self,
        account: &AccountId,
        request: &RequestId,
        amount: Money,
    ) -> Result<(), WalletError<StorageFault>> {
        let mut state = self.lock();
        let existing = state
            .holds
            .get(&(account.clone(), request.clone()))
            .cloned();
        let Some(ExistingHold::Open(open)) = &existing else {
            return settle_closed(&state, account, request, amount, existing);
        };
        let balance_key = (account.clone(), open.amount.currency);
        let Some(balance) = state.balances.get(&balance_key).copied() else {
            return Err(WalletError::Storage(StorageFault::MissingBalance));
        };
        let now = state.now;
        let posted = post_settle(&SettleRequest {
            account,
            request,
            amount,
            at: now,
            balance,
            existing,
        })?;
        state.balances.insert(balance_key, posted.balance);
        if let Some(slot) = state.holds.get_mut(&(account.clone(), request.clone())) {
            *slot = ExistingHold::Settled;
        }
        state.entries.extend(posted.entries);
        Ok(())
    }

    fn release(
        &self,
        account: &AccountId,
        request: &RequestId,
    ) -> Result<(), WalletError<StorageFault>> {
        let mut state = self.lock();
        let existing = state
            .holds
            .get(&(account.clone(), request.clone()))
            .cloned();
        let currency = match &existing {
            Some(ExistingHold::Open(open)) => open.amount.currency,
            Some(ExistingHold::Settled | ExistingHold::Released) | None => {
                return apply_release_without_open(&mut state, account, request, existing);
            }
        };
        let balance_key = (account.clone(), currency);
        let Some(balance) = state.balances.get(&balance_key).copied() else {
            return Err(WalletError::Storage(StorageFault::MissingBalance));
        };
        let now = state.now;
        let posted = post_release(&ReleaseRequest {
            account,
            request,
            at: now,
            balance,
            existing,
        })?;
        state.balances.insert(balance_key, posted.balance);
        if let Some(slot) = state.holds.get_mut(&(account.clone(), request.clone())) {
            *slot = ExistingHold::Released;
        }
        state.entries.extend(posted.entries);
        Ok(())
    }

    fn balance(
        &self,
        account: &AccountId,
        currency: Currency,
    ) -> Result<Money, WalletError<StorageFault>> {
        let state = self.lock();
        state
            .balances
            .get(&(account.clone(), currency))
            .map(|snapshot| snapshot.available)
            .ok_or(WalletError::Storage(StorageFault::MissingBalance))
    }

    fn entries(&self) -> Vec<Entry> {
        self.lock().entries.clone()
    }

    fn funds(&self, currency: Currency) -> Result<CurrencyFunds, WalletError<StorageFault>> {
        let state = self.lock();
        let mut available = 0_u64;
        let mut held = 0_u64;
        let mut deposits = 0_u64;
        for ((_, row_currency), snapshot) in &state.balances {
            if *row_currency != currency {
                continue;
            }
            available = available
                .checked_add(snapshot.available.minor_units)
                .ok_or(WalletError::Rejected(BillingRejection::Overflow))?;
            held = held
                .checked_add(snapshot.held.minor_units)
                .ok_or(WalletError::Rejected(BillingRejection::Overflow))?;
        }
        for ((_, row_currency), amount) in &state.deposits {
            if *row_currency != currency {
                continue;
            }
            deposits = deposits
                .checked_add(*amount)
                .ok_or(WalletError::Rejected(BillingRejection::Overflow))?;
        }
        let revenue = revenue_of(&state.entries, currency)?;
        Ok(CurrencyFunds {
            currency,
            available,
            held,
            deposits,
            revenue,
        })
    }

    fn held(
        &self,
        account: &AccountId,
        request: &RequestId,
    ) -> Result<Money, WalletError<StorageFault>> {
        let state = self.lock();
        match state.holds.get(&(account.clone(), request.clone())) {
            Some(ExistingHold::Open(open)) => Ok(open.amount),
            Some(ExistingHold::Settled | ExistingHold::Released) | None => {
                Err(WalletError::Rejected(BillingRejection::HoldNotFound))
            }
        }
    }

    fn spent(
        &self,
        key: &KeyId,
        currency: Currency,
        since: UnixMillis,
    ) -> Result<Money, WalletError<StorageFault>> {
        let state = self.lock();
        let spent = state.spend(key, currency, since)?;
        spent
            .settled
            .minor_units
            .checked_add(spent.open.minor_units)
            .map(|minor_units| Money {
                currency,
                minor_units,
            })
            .ok_or(WalletError::Rejected(BillingRejection::Overflow))
    }
}

impl Wallet for MemoryWallet {
    type Error = StorageFault;

    fn balance(
        &self,
        account: &AccountId,
        currency: Currency,
    ) -> impl Future<Output = Result<Money, WalletError<Self::Error>>> + Send {
        std::future::ready(WalletContract::balance(self, account, currency))
    }

    fn hold(
        &self,
        account: &AccountId,
        request: &RequestId,
        key: &KeyId,
        cap: Option<KeyCap>,
        amount: Money,
    ) -> impl Future<Output = Result<(), WalletError<Self::Error>>> + Send {
        std::future::ready(WalletContract::hold(
            self, account, request, key, cap, amount,
        ))
    }

    fn settle(
        &self,
        account: &AccountId,
        request: &RequestId,
        amount: Money,
    ) -> impl Future<Output = Result<(), WalletError<Self::Error>>> + Send {
        std::future::ready(WalletContract::settle(self, account, request, amount))
    }

    fn release(
        &self,
        account: &AccountId,
        request: &RequestId,
    ) -> impl Future<Output = Result<(), WalletError<Self::Error>>> + Send {
        std::future::ready(WalletContract::release(self, account, request))
    }

    fn held(
        &self,
        account: &AccountId,
        request: &RequestId,
    ) -> impl Future<Output = Result<Money, WalletError<Self::Error>>> + Send {
        std::future::ready(WalletContract::held(self, account, request))
    }

    fn spent(
        &self,
        key: &KeyId,
        currency: Currency,
        since: UnixMillis,
    ) -> impl Future<Output = Result<Money, WalletError<Self::Error>>> + Send {
        std::future::ready(WalletContract::spent(self, key, currency, since))
    }
}

fn settle_closed(
    state: &State,
    account: &AccountId,
    request: &RequestId,
    amount: Money,
    existing: Option<ExistingHold>,
) -> Result<(), WalletError<StorageFault>> {
    let zero = Money {
        currency: amount.currency,
        minor_units: 0,
    };
    post_settle(&SettleRequest {
        account,
        request,
        amount,
        at: state.now,
        balance: WalletSnapshot {
            available: zero,
            held: zero,
        },
        existing,
    })?;
    Ok(())
}

fn apply_release_without_open(
    state: &mut State,
    account: &AccountId,
    request: &RequestId,
    existing: Option<ExistingHold>,
) -> Result<(), WalletError<StorageFault>> {
    let _ = post_release(&ReleaseRequest {
        account,
        request,
        at: state.now,
        balance: WalletSnapshot {
            available: Money {
                currency: Currency::Usd,
                minor_units: 0,
            },
            held: Money {
                currency: Currency::Usd,
                minor_units: 0,
            },
        },
        existing,
    })?;
    Ok(())
}

impl State {
    fn spend(
        &self,
        key: &KeyId,
        currency: Currency,
        since: UnixMillis,
    ) -> Result<SpendSoFar, WalletError<StorageFault>> {
        let mut settled = 0_u64;
        for entry in &self.entries {
            if entry.amount.currency != currency || entry.at < since {
                continue;
            }
            let Some(entry_key) = &entry.key else {
                continue;
            };
            if entry_key != key {
                continue;
            }
            if entry.direction == Direction::Credit
                && matches!(entry.account, LedgerAccount::Revenue)
            {
                settled = settled
                    .checked_add(entry.amount.minor_units)
                    .ok_or(WalletError::Rejected(BillingRejection::Overflow))?;
            }
        }
        let mut open = 0_u64;
        for hold in self.holds.values() {
            let ExistingHold::Open(row) = hold else {
                continue;
            };
            if row.key != *key || row.amount.currency != currency {
                continue;
            }
            open = open
                .checked_add(row.amount.minor_units)
                .ok_or(WalletError::Rejected(BillingRejection::Overflow))?;
        }
        Ok(SpendSoFar {
            settled: Money {
                currency,
                minor_units: settled,
            },
            open: Money {
                currency,
                minor_units: open,
            },
        })
    }
}

fn revenue_of(entries: &[Entry], currency: Currency) -> Result<u64, WalletError<StorageFault>> {
    let mut total = 0_u64;
    for entry in entries {
        if entry.amount.currency != currency || !matches!(entry.account, LedgerAccount::Revenue) {
            continue;
        }
        total = match entry.direction {
            Direction::Credit => total.checked_add(entry.amount.minor_units),
            Direction::Debit => total.checked_sub(entry.amount.minor_units),
        }
        .ok_or(WalletError::Rejected(BillingRejection::Overflow))?;
    }
    Ok(total)
}

fn zero_spent(currency: Currency) -> SpendSoFar {
    let zero = Money {
        currency,
        minor_units: 0,
    };
    SpendSoFar {
        settled: zero,
        open: zero,
    }
}

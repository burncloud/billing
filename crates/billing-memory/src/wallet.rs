use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use billing::errors::{BillingRejection, WalletError};
use billing::model::{Direction, Entry, KeyCap, LedgerAccount};
use billing::ports::Wallet;
use billing_domain::{
    post_hold, post_release, post_settle, CurrencyFunds, ExistingHold, HoldRequest, OpenHold,
    ReleaseRequest, SettleRequest, SpendSoFar, StorageFault, WalletSnapshot,
};
use kernel::model::{AccountId, Currency, KeyId, Money, RequestId, UnixMillis};
use kernel::ports::Clock;

use crate::contract::WalletContract;

type BalanceKey = (AccountId, Currency);
type HoldKey = (AccountId, RequestId);

#[derive(Debug)]
struct State {
    balances: HashMap<BalanceKey, WalletSnapshot>,
    deposits: HashMap<BalanceKey, u64>,
    holds: HashMap<HoldKey, ExistingHold>,
    entries: Vec<Entry>,
}

/// 测试用的可拨动时钟。
#[derive(Debug)]
pub struct ManualClock {
    now: AtomicU64,
}

impl ManualClock {
    #[must_use]
    pub fn new(now: UnixMillis) -> Self {
        Self {
            now: AtomicU64::new(now.0),
        }
    }
}

impl Clock for ManualClock {
    fn now(&self) -> UnixMillis {
        UnixMillis(self.now.load(Ordering::Relaxed))
    }
}

/// 只用于测试，不作生产实现。
#[derive(Debug)]
pub struct MemoryWallet<C> {
    clock: C,
    state: Mutex<State>,
}

impl<C> MemoryWallet<C> {
    #[must_use]
    pub fn new(clock: C) -> Self {
        Self {
            clock,
            state: Mutex::new(State {
                balances: HashMap::new(),
                deposits: HashMap::new(),
                holds: HashMap::new(),
                entries: Vec::new(),
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

impl Default for MemoryWallet<ManualClock> {
    fn default() -> Self {
        Self::new(ManualClock::new(UnixMillis(0)))
    }
}

impl WalletContract for MemoryWallet<ManualClock> {
    fn set_now(&self, now: UnixMillis) {
        self.clock.now.store(now.0, Ordering::Relaxed);
    }

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
}

impl<C: Clock + Sync> MemoryWallet<C> {
    fn hold_sync(
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
        let now = self.clock.now();
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

    fn settle_sync(
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
            return Err(reject_closed(existing.as_ref()));
        };
        let balance_key = (account.clone(), open.amount.currency);
        let Some(balance) = state.balances.get(&balance_key).copied() else {
            return Err(WalletError::Storage(StorageFault::MissingBalance));
        };
        let now = self.clock.now();
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

    fn release_sync(
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
                return Err(reject_closed(existing.as_ref()));
            }
        };
        let balance_key = (account.clone(), currency);
        let Some(balance) = state.balances.get(&balance_key).copied() else {
            return Err(WalletError::Storage(StorageFault::MissingBalance));
        };
        let now = self.clock.now();
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

    fn balance_sync(
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

    fn held_sync(
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

    fn spent_sync(
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

impl<C: Clock + Sync> Wallet for MemoryWallet<C> {
    type Error = StorageFault;

    fn balance(
        &self,
        account: &AccountId,
        currency: Currency,
    ) -> impl Future<Output = Result<Money, WalletError<Self::Error>>> + Send {
        std::future::ready(Self::balance_sync(self, account, currency))
    }

    fn hold(
        &self,
        account: &AccountId,
        request: &RequestId,
        key: &KeyId,
        cap: Option<KeyCap>,
        amount: Money,
    ) -> impl Future<Output = Result<(), WalletError<Self::Error>>> + Send {
        std::future::ready(Self::hold_sync(self, account, request, key, cap, amount))
    }

    fn settle(
        &self,
        account: &AccountId,
        request: &RequestId,
        amount: Money,
    ) -> impl Future<Output = Result<(), WalletError<Self::Error>>> + Send {
        std::future::ready(Self::settle_sync(self, account, request, amount))
    }

    fn release(
        &self,
        account: &AccountId,
        request: &RequestId,
    ) -> impl Future<Output = Result<(), WalletError<Self::Error>>> + Send {
        std::future::ready(Self::release_sync(self, account, request))
    }

    fn held(
        &self,
        account: &AccountId,
        request: &RequestId,
    ) -> impl Future<Output = Result<Money, WalletError<Self::Error>>> + Send {
        std::future::ready(Self::held_sync(self, account, request))
    }

    fn spent(
        &self,
        key: &KeyId,
        currency: Currency,
        since: UnixMillis,
    ) -> impl Future<Output = Result<Money, WalletError<Self::Error>>> + Send {
        std::future::ready(Self::spent_sync(self, key, currency, since))
    }
}

fn reject_closed(existing: Option<&ExistingHold>) -> WalletError<StorageFault> {
    match existing {
        Some(ExistingHold::Settled) => WalletError::Rejected(BillingRejection::AlreadySettled),
        Some(ExistingHold::Released) => WalletError::Rejected(BillingRejection::AlreadyReleased),
        Some(ExistingHold::Open(_)) | None => WalletError::Rejected(BillingRejection::HoldNotFound),
    }
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

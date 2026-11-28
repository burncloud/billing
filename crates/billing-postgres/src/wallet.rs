use billing::errors::{BillingRejection, WalletError};
use billing::model::{Direction, Entry, EntryRef, HoldId, KeyCap, LedgerAccount, PendingHold};
use billing::ports::Wallet;
use billing_domain::{
    post_hold, post_release, post_settle, ExistingHold, HoldRequest, OpenHold, ReleaseRequest,
    SettleRequest, SpendSoFar, StorageFault, WalletSnapshot,
};
use kernel::model::{AccountId, Currency, KeyId, Money, RequestId, UnixMillis};
use kernel::ports::Clock;
use sqlx::migrate::{Migration, MigrationType, Migrator};
use sqlx::Row;
use sqlx::SqlSafeStr;

use crate::PostgresError;

mod accounts;

type Tx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;

/// Postgres 上的钱包。判定调用 `billing-domain`，分录时间用 `clock.now()`。
pub struct PostgresWallet<C> {
    pool: sqlx::PgPool,
    clock: C,
}

impl<C> PostgresWallet<C> {
    #[must_use]
    pub fn new(pool: sqlx::PgPool, clock: C) -> Self {
        Self { pool, clock }
    }

    /// 执行本 crate 的全部迁移，包括 `model_prices` 表。
    ///
    /// # Errors
    ///
    /// 迁移执行失败时返回 `PostgresError::Migrate`。
    pub async fn migrate(pool: &sqlx::PgPool) -> Result<(), PostgresError> {
        run_migrations(pool, migrations()).await
    }
}

fn migrations() -> Vec<Migration> {
    vec![
        Migration::new(
            1,
            "wallet".into(),
            MigrationType::Simple,
            include_str!("../migrations/0001_wallet.sql").into_sql_str(),
            false,
        ),
        Migration::new(
            2,
            "model_prices".into(),
            MigrationType::Simple,
            include_str!("../migrations/0002_model_prices.sql").into_sql_str(),
            false,
        ),
        Migration::new(
            3,
            "hold_created_at".into(),
            MigrationType::Simple,
            include_str!("../migrations/0003_hold_created_at.sql").into_sql_str(),
            false,
        ),
        Migration::new(
            4,
            "credits".into(),
            MigrationType::Simple,
            include_str!("../migrations/0004_credits.sql").into_sql_str(),
            false,
        ),
        Migration::new(
            5,
            "funding_entries".into(),
            MigrationType::Simple,
            include_str!("../migrations/0005_funding_entries.sql").into_sql_str(),
            false,
        ),
        Migration::new(
            6,
            "hold_token".into(),
            MigrationType::Simple,
            include_str!("../migrations/0006_hold_token.sql").into_sql_str(),
            false,
        ),
    ]
}

async fn run_migrations(
    pool: &sqlx::PgPool,
    migrations: Vec<Migration>,
) -> Result<(), PostgresError> {
    Migrator::with_migrations(migrations)
        .run(pool)
        .await
        .map_err(|error| PostgresError::Migrate(Box::new(error)))?;
    Ok(())
}

impl<C: Clock + Sync> Wallet for PostgresWallet<C> {
    type Error = PostgresError;

    async fn balance(
        &self,
        account: &AccountId,
        currency: Currency,
    ) -> Result<Money, WalletError<Self::Error>> {
        let row =
            sqlx::query("SELECT available FROM balances WHERE account = $1 AND currency = $2")
                .bind(account.0.as_str())
                .bind(currency_code(currency))
                .fetch_optional(&self.pool)
                .await
                .map_err(storage)?;
        match row {
            None => Err(WalletError::Storage(PostgresError::MissingBalance)),
            Some(row) => Ok(money(currency, minor_from_row(&row, "available")?)),
        }
    }

    async fn hold(
        &self,
        account: &AccountId,
        request: &RequestId,
        key: &KeyId,
        cap: Option<KeyCap>,
        amount: Money,
    ) -> Result<HoldId, WalletError<Self::Error>> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let result = hold_in(
            &mut tx,
            self.clock.now(),
            account,
            request,
            key,
            cap,
            amount,
        )
        .await;
        finish(tx, result).await
    }

    async fn settle(
        &self,
        account: &AccountId,
        request: &RequestId,
        hold: &HoldId,
        amount: Money,
    ) -> Result<(), WalletError<Self::Error>> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let result = settle_in(&mut tx, self.clock.now(), account, request, hold, amount).await;
        finish(tx, result).await
    }

    async fn release(
        &self,
        account: &AccountId,
        request: &RequestId,
        hold: &HoldId,
    ) -> Result<(), WalletError<Self::Error>> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let result = release_in(&mut tx, self.clock.now(), account, request, hold).await;
        finish(tx, result).await
    }

    async fn held(
        &self,
        account: &AccountId,
        request: &RequestId,
    ) -> Result<Money, WalletError<Self::Error>> {
        let row = sqlx::query(
                "SELECT hold_key, currency, amount, state FROM holds WHERE account = $1 AND request = $2",
            )
            .bind(account.0.as_str())
            .bind(request.0.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(storage)?;
        match row {
            Some(row) => match hold_from_row(&row)? {
                ExistingHold::Open(open) => Ok(open.amount),
                ExistingHold::Settled | ExistingHold::Released => {
                    Err(rejected(BillingRejection::HoldNotFound))
                }
            },
            None => Err(rejected(BillingRejection::HoldNotFound)),
        }
    }

    async fn spent(
        &self,
        key: &KeyId,
        currency: Currency,
        since: UnixMillis,
    ) -> Result<Money, WalletError<Self::Error>> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        lock_key(&mut tx, key).await?;
        let spent = spend_in(&mut tx, key, currency, since).await?;
        tx.commit().await.map_err(storage)?;
        let minor_units = spent
            .settled
            .minor_units
            .checked_add(spent.open.minor_units)
            .ok_or(rejected(BillingRejection::Overflow))?;
        Ok(money(currency, minor_units))
    }

    async fn open_holds_created_before(
        &self,
        cutoff: UnixMillis,
    ) -> Result<Vec<PendingHold>, WalletError<Self::Error>> {
        self.list_open_holds(cutoff)
            .await
            .map_err(WalletError::Storage)
    }
}

async fn finish<T>(
    tx: Tx<'_>,
    result: Result<T, WalletError<PostgresError>>,
) -> Result<T, WalletError<PostgresError>> {
    match result {
        Ok(value) => {
            tx.commit().await.map_err(storage)?;
            Ok(value)
        }
        Err(error) => Err(error),
    }
}

async fn hold_in(
    tx: &mut Tx<'_>,
    now: UnixMillis,
    account: &AccountId,
    request: &RequestId,
    key: &KeyId,
    cap: Option<KeyCap>,
    amount: Money,
) -> Result<HoldId, WalletError<PostgresError>> {
    lock_key(tx, key).await?;
    let existing = lock_hold(tx, account, request).await?.map(|(hold, _)| hold);
    let balance = lock_balance(tx, account, amount.currency).await?;
    let spent = match cap {
        Some(cap) => spend_in(tx, key, amount.currency, cap.since).await?,
        None => zero_spent(amount.currency),
    };
    let posted = from_domain(post_hold(&HoldRequest {
        account,
        request,
        key,
        cap,
        amount,
        at: now,
        balance,
        existing,
        spent,
    }))?;
    write_balance(tx, account, posted.balance).await?;
    let hold_id = insert_open_hold(tx, now, account, request, key, amount).await?;
    insert_entries(tx, &posted.entries)
        .await
        .map_err(WalletError::Storage)?;
    Ok(hold_id)
}

async fn settle_in(
    tx: &mut Tx<'_>,
    now: UnixMillis,
    account: &AccountId,
    request: &RequestId,
    hold: &HoldId,
    amount: Money,
) -> Result<(), WalletError<PostgresError>> {
    let Some(key) = peek_key(tx, account, request).await? else {
        return Err(rejected(BillingRejection::HoldNotFound));
    };
    lock_key(tx, &key).await?;
    let Some((existing, stored)) = lock_hold(tx, account, request).await? else {
        return Err(reject_closed(None));
    };
    let currency = open_currency(&existing, &stored, hold)?;
    let Some(balance) = lock_balance(tx, account, currency).await? else {
        return Err(WalletError::Storage(PostgresError::MissingBalance));
    };
    let posted = from_domain(post_settle(&SettleRequest {
        account,
        request,
        amount,
        at: now,
        balance,
        existing: Some(existing),
    }))?;
    write_balance(tx, account, posted.balance).await?;
    set_hold_state(tx, account, request, "settled").await?;
    insert_entries(tx, &posted.entries)
        .await
        .map_err(WalletError::Storage)?;
    Ok(())
}

async fn release_in(
    tx: &mut Tx<'_>,
    now: UnixMillis,
    account: &AccountId,
    request: &RequestId,
    hold: &HoldId,
) -> Result<(), WalletError<PostgresError>> {
    let Some(key) = peek_key(tx, account, request).await? else {
        return Err(rejected(BillingRejection::HoldNotFound));
    };
    lock_key(tx, &key).await?;
    let Some((existing, stored)) = lock_hold(tx, account, request).await? else {
        return Err(reject_closed(None));
    };
    let currency = open_currency(&existing, &stored, hold)?;
    let Some(balance) = lock_balance(tx, account, currency).await? else {
        return Err(WalletError::Storage(PostgresError::MissingBalance));
    };
    let posted = from_domain(post_release(&ReleaseRequest {
        account,
        request,
        at: now,
        balance,
        existing: Some(existing),
    }))?;
    write_balance(tx, account, posted.balance).await?;
    set_hold_state(tx, account, request, "released").await?;
    insert_entries(tx, &posted.entries)
        .await
        .map_err(WalletError::Storage)?;
    Ok(())
}

fn open_currency(
    existing: &ExistingHold,
    stored: &HoldId,
    presented: &HoldId,
) -> Result<Currency, WalletError<PostgresError>> {
    let ExistingHold::Open(open) = existing else {
        return Err(reject_closed(Some(existing)));
    };
    if stored != presented {
        return Err(rejected(BillingRejection::HoldMismatch));
    }
    Ok(open.amount.currency)
}

async fn lock_key(tx: &mut Tx<'_>, key: &KeyId) -> Result<(), WalletError<PostgresError>> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(key.0.as_str())
        .execute(&mut **tx)
        .await
        .map_err(storage)?;
    Ok(())
}

async fn lock_balance(
    tx: &mut Tx<'_>,
    account: &AccountId,
    currency: Currency,
) -> Result<Option<WalletSnapshot>, WalletError<PostgresError>> {
    let row = sqlx::query(
        "SELECT available, held FROM balances WHERE account = $1 AND currency = $2 FOR UPDATE",
    )
    .bind(account.0.as_str())
    .bind(currency_code(currency))
    .fetch_optional(&mut **tx)
    .await
    .map_err(storage)?;
    match row {
        None => Ok(None),
        Some(row) => Ok(Some(WalletSnapshot {
            available: money(currency, minor_from_row(&row, "available")?),
            held: money(currency, minor_from_row(&row, "held")?),
        })),
    }
}

async fn peek_key(
    tx: &mut Tx<'_>,
    account: &AccountId,
    request: &RequestId,
) -> Result<Option<KeyId>, WalletError<PostgresError>> {
    let key: Option<String> =
        sqlx::query_scalar("SELECT hold_key FROM holds WHERE account = $1 AND request = $2")
            .bind(account.0.as_str())
            .bind(request.0.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(storage)?;
    Ok(key.map(KeyId))
}

async fn lock_hold(
    tx: &mut Tx<'_>,
    account: &AccountId,
    request: &RequestId,
) -> Result<Option<(ExistingHold, HoldId)>, WalletError<PostgresError>> {
    let row = sqlx::query(
        "SELECT hold_key, currency, amount, state, hold_token FROM holds WHERE account = $1 AND request = $2 FOR UPDATE",
    )
    .bind(account.0.as_str())
    .bind(request.0.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(storage)?;
    match row {
        None => Ok(None),
        Some(row) => {
            let token = HoldId(row.try_get::<String, _>("hold_token").map_err(storage)?);
            Ok(Some((hold_from_row(&row)?, token)))
        }
    }
}

async fn spend_in(
    tx: &mut Tx<'_>,
    key: &KeyId,
    currency: Currency,
    since: UnixMillis,
) -> Result<SpendSoFar, WalletError<PostgresError>> {
    let settled_rows = settled_amounts(&mut **tx, key, currency, since).await?;
    let open_rows = open_amounts(&mut **tx, key, currency).await?;
    fold_spend(settled_rows, open_rows, currency)
}

async fn settled_amounts<'e, E>(
    executor: E,
    key: &KeyId,
    currency: Currency,
    since: UnixMillis,
) -> Result<Vec<sqlx::postgres::PgRow>, WalletError<PostgresError>>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(
        "SELECT amount FROM entries WHERE entry_key = $1 AND currency = $2 AND direction = 'credit' AND account_kind = 'revenue' AND at >= $3",
    )
    .bind(key.0.as_str())
    .bind(currency_code(currency))
    .bind(to_i64(since.0)?)
    .fetch_all(executor)
    .await
    .map_err(storage)
}

async fn open_amounts<'e, E>(
    executor: E,
    key: &KeyId,
    currency: Currency,
) -> Result<Vec<sqlx::postgres::PgRow>, WalletError<PostgresError>>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query("SELECT amount FROM holds WHERE hold_key = $1 AND currency = $2 AND state = 'open'")
        .bind(key.0.as_str())
        .bind(currency_code(currency))
        .fetch_all(executor)
        .await
        .map_err(storage)
}

fn fold_spend(
    settled_rows: Vec<sqlx::postgres::PgRow>,
    open_rows: Vec<sqlx::postgres::PgRow>,
    currency: Currency,
) -> Result<SpendSoFar, WalletError<PostgresError>> {
    let mut settled = 0_u64;
    for row in settled_rows {
        settled = settled
            .checked_add(minor_from_row(&row, "amount")?)
            .ok_or(rejected(BillingRejection::Overflow))?;
    }
    let mut open = 0_u64;
    for row in open_rows {
        open = open
            .checked_add(minor_from_row(&row, "amount")?)
            .ok_or(rejected(BillingRejection::Overflow))?;
    }
    Ok(SpendSoFar {
        settled: money(currency, settled),
        open: money(currency, open),
    })
}

async fn write_balance(
    tx: &mut Tx<'_>,
    account: &AccountId,
    balance: WalletSnapshot,
) -> Result<(), WalletError<PostgresError>> {
    let changed = sqlx::query(
        "UPDATE balances SET available = $3, held = $4 WHERE account = $1 AND currency = $2",
    )
    .bind(account.0.as_str())
    .bind(currency_code(balance.available.currency))
    .bind(to_i64(balance.available.minor_units)?)
    .bind(to_i64(balance.held.minor_units)?)
    .execute(&mut **tx)
    .await
    .map_err(storage)?
    .rows_affected();
    if changed == 0 {
        return Err(WalletError::Storage(PostgresError::MissingBalance));
    }
    Ok(())
}

async fn insert_open_hold(
    tx: &mut Tx<'_>,
    now: UnixMillis,
    account: &AccountId,
    request: &RequestId,
    key: &KeyId,
    amount: Money,
) -> Result<HoldId, WalletError<PostgresError>> {
    let result = sqlx::query_scalar(
        "INSERT INTO holds (account, request, hold_key, currency, amount, state, created_at, hold_token) VALUES ($1, $2, $3, $4, $5, 'open', $6, gen_random_uuid()::text) RETURNING hold_token",
    )
    .bind(account.0.as_str())
    .bind(request.0.as_str())
    .bind(key.0.as_str())
    .bind(currency_code(amount.currency))
    .bind(to_i64(amount.minor_units)?)
    .bind(clock_i64(now)?)
    .fetch_one(&mut **tx)
    .await;
    match result {
        Ok(token) => Ok(HoldId(token)),
        Err(error) if is_unique(&error) => Err(rejected(BillingRejection::DuplicateRequest)),
        Err(error) => Err(storage(error)),
    }
}

async fn set_hold_state(
    tx: &mut Tx<'_>,
    account: &AccountId,
    request: &RequestId,
    state: &str,
) -> Result<(), WalletError<PostgresError>> {
    sqlx::query("UPDATE holds SET state = $3 WHERE account = $1 AND request = $2")
        .bind(account.0.as_str())
        .bind(request.0.as_str())
        .bind(state)
        .execute(&mut **tx)
        .await
        .map_err(storage)?;
    Ok(())
}

pub(super) async fn insert_entries(
    tx: &mut Tx<'_>,
    entries: &[Entry],
) -> Result<(), PostgresError> {
    for entry in entries {
        let (kind, account_id) = match &entry.account {
            LedgerAccount::Available(id) => ("available", Some(id.0.as_str())),
            LedgerAccount::Held(id) => ("held", Some(id.0.as_str())),
            LedgerAccount::Revenue => ("revenue", None),
            LedgerAccount::Funding => ("funding", None),
        };
        let (reference_kind, reference) = match &entry.reference {
            EntryRef::Request(id) => ("request", id.0.as_str()),
            EntryRef::Credit(id) => ("credit", id.0.as_str()),
        };
        let direction = match entry.direction {
            Direction::Debit => "debit",
            Direction::Credit => "credit",
        };
        sqlx::query(
            "INSERT INTO entries (reference, reference_kind, account_kind, account_id, entry_key, direction, currency, amount, at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(reference)
        .bind(reference_kind)
        .bind(kind)
        .bind(account_id)
        .bind(entry.key.as_ref().map(|key| key.0.as_str()))
        .bind(direction)
        .bind(currency_code(entry.amount.currency))
        .bind(i64::try_from(entry.amount.minor_units).map_err(|_| PostgresError::AmountOutOfRange)?)
        .bind(i64::try_from(entry.at.0).map_err(|_| PostgresError::ClockOutOfRange)?)
        .execute(&mut **tx)
        .await
        .map_err(database)?;
    }
    Ok(())
}

pub(super) fn database(error: sqlx::Error) -> PostgresError {
    PostgresError::Database(Box::new(error))
}

fn hold_from_row(row: &sqlx::postgres::PgRow) -> Result<ExistingHold, WalletError<PostgresError>> {
    let state: String = row.try_get("state").map_err(storage)?;
    match state.as_str() {
        "settled" => Ok(ExistingHold::Settled),
        "released" => Ok(ExistingHold::Released),
        "open" => {
            let key = KeyId(row.try_get::<String, _>("hold_key").map_err(storage)?);
            let currency = parse_currency(&row.try_get::<String, _>("currency").map_err(storage)?)
                .map_err(WalletError::Storage)?;
            let amount = minor_from_row(row, "amount")?;
            Ok(ExistingHold::Open(OpenHold {
                key,
                amount: money(currency, amount),
            }))
        }
        _ => Err(WalletError::Storage(PostgresError::UnknownHold)),
    }
}

#[cfg(test)]
fn entry_from_row(row: &sqlx::postgres::PgRow) -> Result<Entry, WalletError<PostgresError>> {
    let kind: String = row.try_get("account_kind").map_err(storage)?;
    let account_id: Option<String> = row.try_get("account_id").map_err(storage)?;
    let account = match (kind.as_str(), account_id) {
        ("available", Some(id)) => LedgerAccount::Available(AccountId(id)),
        ("held", Some(id)) => LedgerAccount::Held(AccountId(id)),
        ("revenue", None) => LedgerAccount::Revenue,
        ("funding", None) => LedgerAccount::Funding,
        _ => return Err(WalletError::Storage(PostgresError::UnknownEntry)),
    };
    let direction = match row
        .try_get::<String, _>("direction")
        .map_err(storage)?
        .as_str()
    {
        "debit" => Direction::Debit,
        "credit" => Direction::Credit,
        _ => return Err(WalletError::Storage(PostgresError::UnknownEntry)),
    };
    let currency = parse_currency(&row.try_get::<String, _>("currency").map_err(storage)?)
        .map_err(WalletError::Storage)?;
    let entry_key: Option<String> = row.try_get("entry_key").map_err(storage)?;
    let at: i64 = row.try_get("at").map_err(storage)?;
    let at =
        u64::try_from(at).map_err(|_| WalletError::Storage(PostgresError::AmountOutOfRange))?;
    let reference: String = row.try_get("reference").map_err(storage)?;
    let reference_kind: String = row.try_get("reference_kind").map_err(storage)?;
    let reference = match reference_kind.as_str() {
        "request" => EntryRef::Request(RequestId(reference)),
        "credit" => EntryRef::Credit(kernel::model::CreditId(reference)),
        _ => return Err(WalletError::Storage(PostgresError::UnknownEntry)),
    };
    Ok(Entry {
        reference,
        account,
        key: entry_key.map(KeyId),
        direction,
        amount: money(currency, minor_from_row(row, "amount")?),
        at: UnixMillis(at),
    })
}

fn from_domain<T>(
    result: Result<T, WalletError<StorageFault>>,
) -> Result<T, WalletError<PostgresError>> {
    match result {
        Ok(value) => Ok(value),
        Err(WalletError::Rejected(reason)) => Err(WalletError::Rejected(reason)),
        Err(WalletError::Storage(StorageFault::MissingBalance)) => {
            Err(WalletError::Storage(PostgresError::MissingBalance))
        }
        Err(WalletError::Storage(StorageFault::HeldBelowOpen)) => {
            Err(WalletError::Storage(PostgresError::HeldBelowOpen))
        }
    }
}

fn reject_closed(existing: Option<&ExistingHold>) -> WalletError<PostgresError> {
    match existing {
        Some(ExistingHold::Settled) => rejected(BillingRejection::AlreadySettled),
        Some(ExistingHold::Released) => rejected(BillingRejection::AlreadyReleased),
        Some(ExistingHold::Open(_)) | None => rejected(BillingRejection::HoldNotFound),
    }
}

fn minor_from_row(
    row: &sqlx::postgres::PgRow,
    name: &str,
) -> Result<u64, WalletError<PostgresError>> {
    let value: i64 = row.try_get(name).map_err(storage)?;
    u64::try_from(value).map_err(|_| WalletError::Storage(PostgresError::AmountOutOfRange))
}

fn to_i64(value: u64) -> Result<i64, WalletError<PostgresError>> {
    i64::try_from(value).map_err(|_| rejected(BillingRejection::Overflow))
}

fn clock_i64(now: UnixMillis) -> Result<i64, WalletError<PostgresError>> {
    i64::try_from(now.0).map_err(|_| WalletError::Storage(PostgresError::ClockOutOfRange))
}

fn storage(error: sqlx::Error) -> WalletError<PostgresError> {
    WalletError::Storage(PostgresError::Database(Box::new(error)))
}

fn rejected(reason: BillingRejection) -> WalletError<PostgresError> {
    WalletError::Rejected(reason)
}

fn is_unique(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .is_some_and(|code| code == "23505")
}

fn currency_code(currency: Currency) -> &'static str {
    match currency {
        Currency::Usd => "usd",
        Currency::Cny => "cny",
    }
}

pub(crate) fn parse_currency(value: &str) -> Result<Currency, PostgresError> {
    match value {
        "usd" => Ok(Currency::Usd),
        "cny" => Ok(Currency::Cny),
        _ => Err(PostgresError::UnknownCurrency),
    }
}

fn money(currency: Currency, minor_units: u64) -> Money {
    Money {
        currency,
        minor_units,
    }
}

fn zero_spent(currency: Currency) -> SpendSoFar {
    SpendSoFar {
        settled: money(currency, 0),
        open: money(currency, 0),
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::atomic::{AtomicU64, Ordering};

    use billing::errors::{BillingRejection, WalletError};
    use billing::model::HoldId;
    use billing::ports::{Funding, Wallet};
    use billing_domain::{
        credit_entries, CreditId, Credited, FundingError, Opened, PendingHold, StorageFault,
    };
    use billing_memory::{run_wallet_contract, DriveWallet, WalletContract};
    use kernel::model::{AccountId, Currency, KeyId, Money, RequestId, UnixMillis};
    use kernel::ports::Clock;
    use sqlx::Row;

    use crate::testdb::{database_url, schema_pool};

    use super::{
        currency_code, entry_from_row, finish, minor_from_row, rejected, storage, to_i64,
        PostgresError, PostgresWallet,
    };

    static SEEDS: AtomicU64 = AtomicU64::new(0);

    struct TestClock {
        now: AtomicU64,
    }

    impl TestClock {
        fn new(now: UnixMillis) -> Self {
            Self {
                now: AtomicU64::new(now.0),
            }
        }

        fn set(&self, now: UnixMillis) {
            self.now.store(now.0, Ordering::Relaxed);
        }
    }

    impl Clock for TestClock {
        fn now(&self) -> UnixMillis {
            UnixMillis(self.now.load(Ordering::Relaxed))
        }
    }

    impl<C: Clock + Sync> PostgresWallet<C> {
        async fn deposit(
            &self,
            account: &AccountId,
            amount: Money,
        ) -> Result<(), WalletError<PostgresError>> {
            let mut tx = self.pool.begin().await.map_err(storage)?;
            let result = deposit_in(&mut tx, self.clock.now(), account, amount).await;
            finish(tx, result).await
        }

        async fn entries(&self) -> Result<Vec<billing::model::Entry>, WalletError<PostgresError>> {
            let rows = sqlx::query(
                "SELECT reference, reference_kind, account_kind, account_id, entry_key, direction, currency, amount, at FROM entries ORDER BY id",
            )
            .fetch_all(&self.pool)
            .await
            .map_err(storage)?;
            let mut entries = Vec::new();
            for row in rows {
                entries.push(entry_from_row(&row)?);
            }
            Ok(entries)
        }

        async fn funds(
            &self,
            currency: Currency,
        ) -> Result<billing_domain::CurrencyFunds, WalletError<PostgresError>> {
            let balances = sqlx::query("SELECT available, held FROM balances WHERE currency = $1")
                .bind(currency_code(currency))
                .fetch_all(&self.pool)
                .await
                .map_err(storage)?;
            let mut available = 0_u64;
            let mut held = 0_u64;
            for row in balances {
                available = available
                    .checked_add(minor_from_row(&row, "available")?)
                    .ok_or(rejected(BillingRejection::Overflow))?;
                held = held
                    .checked_add(minor_from_row(&row, "held")?)
                    .ok_or(rejected(BillingRejection::Overflow))?;
            }
            let revenue_rows = sqlx::query(
                "SELECT direction, amount FROM entries WHERE currency = $1 AND account_kind = 'revenue'",
            )
            .bind(currency_code(currency))
            .fetch_all(&self.pool)
            .await
            .map_err(storage)?;
            let mut revenue = 0_u64;
            for row in revenue_rows {
                let amount = minor_from_row(&row, "amount")?;
                let direction: String = row.try_get("direction").map_err(storage)?;
                revenue = match direction.as_str() {
                    "credit" => revenue.checked_add(amount),
                    "debit" => revenue.checked_sub(amount),
                    _ => return Err(WalletError::Storage(PostgresError::UnknownEntry)),
                }
                .ok_or(rejected(BillingRejection::Overflow))?;
            }
            Ok(billing_domain::CurrencyFunds {
                currency,
                available,
                held,
                revenue,
            })
        }

        async fn account_funds(
            &self,
        ) -> Result<Vec<billing_domain::AccountFunds>, WalletError<PostgresError>> {
            let rows = sqlx::query(
                "SELECT account, currency, available, held FROM balances ORDER BY account COLLATE \"C\", currency",
            )
            .fetch_all(&self.pool)
            .await
            .map_err(storage)?;
            let mut accounts = Vec::new();
            for row in rows {
                let currency: String = row.try_get("currency").map_err(storage)?;
                accounts.push(billing_domain::AccountFunds {
                    account: AccountId(row.try_get("account").map_err(storage)?),
                    currency: super::parse_currency(&currency).map_err(WalletError::Storage)?,
                    available: minor_from_row(&row, "available")?,
                    held: minor_from_row(&row, "held")?,
                });
            }
            Ok(accounts)
        }
    }

    async fn deposit_in(
        tx: &mut super::Tx<'_>,
        now: UnixMillis,
        account: &AccountId,
        amount: Money,
    ) -> Result<(), WalletError<PostgresError>> {
        sqlx::query(
            "INSERT INTO balances (account, currency, available, held, deposited) VALUES ($1, $2, 0, 0, 0) ON CONFLICT DO NOTHING",
        )
        .bind(account.0.as_str())
        .bind(currency_code(amount.currency))
        .execute(&mut **tx)
        .await
        .map_err(storage)?;
        let row = sqlx::query(
            "SELECT available, deposited FROM balances WHERE account = $1 AND currency = $2 FOR UPDATE",
        )
        .bind(account.0.as_str())
        .bind(currency_code(amount.currency))
        .fetch_optional(&mut **tx)
        .await
        .map_err(storage)?;
        let Some(row) = row else {
            return Err(WalletError::Storage(PostgresError::MissingBalance));
        };
        let available = minor_from_row(&row, "available")?
            .checked_add(amount.minor_units)
            .ok_or(rejected(BillingRejection::Overflow))?;
        let deposited = minor_from_row(&row, "deposited")?
            .checked_add(amount.minor_units)
            .ok_or(rejected(BillingRejection::Overflow))?;
        sqlx::query(
            "UPDATE balances SET available = $3, deposited = $4 WHERE account = $1 AND currency = $2",
        )
        .bind(account.0.as_str())
        .bind(currency_code(amount.currency))
        .bind(to_i64(available)?)
        .bind(to_i64(deposited)?)
        .execute(&mut **tx)
        .await
        .map_err(storage)?;
        let seed = CreditId(format!("seed-{}", SEEDS.fetch_add(1, Ordering::Relaxed)));
        super::insert_entries(tx, &credit_entries(account, &seed, amount, now))
            .await
            .map_err(WalletError::Storage)?;
        Ok(())
    }

    struct PgContract {
        wallet: PostgresWallet<TestClock>,
        handle: tokio::runtime::Handle,
    }

    impl PgContract {
        fn call<T>(&self, future: impl Future<Output = T> + Send) -> T {
            self.handle.block_on(future)
        }
    }

    impl Wallet for PgContract {
        type Error = StorageFault;

        fn balance(
            &self,
            account: &kernel::model::AccountId,
            currency: Currency,
        ) -> impl Future<Output = Result<Money, WalletError<Self::Error>>> + Send {
            std::future::ready(adapt(self.call(self.wallet.balance(account, currency))))
        }

        fn hold(
            &self,
            account: &kernel::model::AccountId,
            request: &kernel::model::RequestId,
            key: &kernel::model::KeyId,
            cap: Option<billing::model::KeyCap>,
            amount: Money,
        ) -> impl Future<Output = Result<HoldId, WalletError<Self::Error>>> + Send {
            std::future::ready(adapt(
                self.call(self.wallet.hold(account, request, key, cap, amount)),
            ))
        }

        fn settle(
            &self,
            account: &kernel::model::AccountId,
            request: &kernel::model::RequestId,
            hold: &HoldId,
            amount: Money,
        ) -> impl Future<Output = Result<(), WalletError<Self::Error>>> + Send {
            std::future::ready(adapt(
                self.call(self.wallet.settle(account, request, hold, amount)),
            ))
        }

        fn release(
            &self,
            account: &kernel::model::AccountId,
            request: &kernel::model::RequestId,
            hold: &HoldId,
        ) -> impl Future<Output = Result<(), WalletError<Self::Error>>> + Send {
            std::future::ready(adapt(
                self.call(self.wallet.release(account, request, hold)),
            ))
        }

        fn held(
            &self,
            account: &kernel::model::AccountId,
            request: &kernel::model::RequestId,
        ) -> impl Future<Output = Result<Money, WalletError<Self::Error>>> + Send {
            std::future::ready(adapt(self.call(self.wallet.held(account, request))))
        }

        fn spent(
            &self,
            key: &kernel::model::KeyId,
            currency: Currency,
            since: UnixMillis,
        ) -> impl Future<Output = Result<Money, WalletError<Self::Error>>> + Send {
            std::future::ready(adapt(self.call(self.wallet.spent(key, currency, since))))
        }

        fn open_holds_created_before(
            &self,
            cutoff: UnixMillis,
        ) -> impl Future<Output = Result<Vec<PendingHold>, WalletError<Self::Error>>> + Send
        {
            std::future::ready(adapt(
                self.call(Wallet::open_holds_created_before(&self.wallet, cutoff)),
            ))
        }
    }

    impl WalletContract for PgContract {
        fn deposit(
            &self,
            account: &AccountId,
            amount: Money,
        ) -> Result<(), WalletError<StorageFault>> {
            adapt(self.call(self.wallet.deposit(account, amount)))
        }

        fn entries(&self) -> Vec<billing::model::Entry> {
            match self.call(self.wallet.entries()) {
                Ok(entries) => entries,
                Err(error) => panic!("{error:?}"),
            }
        }

        fn funds(
            &self,
            currency: Currency,
        ) -> Result<billing_domain::CurrencyFunds, WalletError<StorageFault>> {
            adapt(self.call(self.wallet.funds(currency)))
        }

        fn account_funds(&self) -> Vec<billing_domain::AccountFunds> {
            match self.call(self.wallet.account_funds()) {
                Ok(accounts) => accounts,
                Err(error) => panic!("{error:?}"),
            }
        }

        fn set_now(&self, now: UnixMillis) {
            self.wallet.clock.set(now);
        }

        fn open_account(
            &self,
            account: &AccountId,
            currency: Currency,
        ) -> Result<Opened, FundingError<StorageFault>> {
            match self.call(Funding::open_account(&self.wallet, account, currency)) {
                Ok(opened) => Ok(opened),
                Err(FundingError::Rejected(reason)) => Err(FundingError::Rejected(reason)),
                Err(FundingError::Storage(error)) => panic!("{error:?}"),
            }
        }

        fn credit(
            &self,
            account: &AccountId,
            credit: &CreditId,
            amount: Money,
        ) -> Result<Credited, FundingError<StorageFault>> {
            match self.call(Funding::credit(&self.wallet, account, credit, amount)) {
                Ok(credited) => Ok(credited),
                Err(FundingError::Rejected(reason)) => Err(FundingError::Rejected(reason)),
                Err(FundingError::Storage(error)) => panic!("{error:?}"),
            }
        }
    }

    struct TokioDrive {
        runtime: tokio::runtime::Runtime,
    }

    impl TokioDrive {
        fn start() -> Self {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_io()
                .enable_time()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => panic!("{error}"),
            };
            Self { runtime }
        }
    }

    impl DriveWallet for TokioDrive {
        fn run<T, F>(&self, future: F) -> T
        where
            F: Future<Output = T> + Send,
        {
            self.runtime.handle().block_on(future)
        }
    }

    async fn open_store(url: &str, handle: tokio::runtime::Handle) -> PgContract {
        let pool = schema_pool(url).await;
        if let Err(error) = PostgresWallet::<TestClock>::migrate(&pool).await {
            panic!("{error}");
        }
        PgContract {
            wallet: PostgresWallet::new(pool, TestClock::new(UnixMillis(0))),
            handle,
        }
    }

    fn adapt<T>(
        result: Result<T, WalletError<PostgresError>>,
    ) -> Result<T, WalletError<StorageFault>> {
        match result {
            Ok(value) => Ok(value),
            Err(WalletError::Rejected(reason)) => Err(WalletError::Rejected(reason)),
            Err(WalletError::Storage(PostgresError::MissingBalance)) => {
                Err(WalletError::Storage(StorageFault::MissingBalance))
            }
            Err(WalletError::Storage(PostgresError::HeldBelowOpen)) => {
                Err(WalletError::Storage(StorageFault::HeldBelowOpen))
            }
            Err(error) => panic!("{error:?}"),
        }
    }

    #[test]
    fn postgres_wallet_matches_the_contract() {
        let url = database_url();
        let drive = TokioDrive::start();
        let handle = drive.runtime.handle().clone();
        run_wallet_contract(move || drive_open(&url, &handle), &drive);
    }

    fn drive_open(url: &str, handle: &tokio::runtime::Handle) -> PgContract {
        handle.block_on(open_store(url, handle.clone()))
    }

    fn books(store: &PgContract) -> Result<(), billing_domain::InvariantViolation> {
        let funds = match store.funds(Currency::Usd) {
            Ok(funds) => funds,
            Err(error) => panic!("{error:?}"),
        };
        billing_domain::check_ledger(&store.entries(), &[funds], &store.account_funds())
    }

    fn tamper(store: &PgContract, sql: &'static str) {
        if let Err(error) = store.call(sqlx::query(sql).execute(&store.wallet.pool)) {
            panic!("{error:?}");
        }
    }

    #[test]
    fn the_ledger_check_catches_a_tampered_wallet() {
        let url = database_url();
        let drive = TokioDrive::start();
        let store = drive_open(&url, drive.runtime.handle());
        let first = AccountId(String::from("t1"));
        let second = AccountId(String::from("t2"));
        let key = KeyId(String::from("tamper-key"));
        let usd = |minor_units| Money {
            currency: Currency::Usd,
            minor_units,
        };
        for (account, id, amount) in [(&first, "c-1", 100), (&second, "c-2", 50)] {
            match store.call(store.wallet.open_account(account, Currency::Usd)) {
                Ok(_) => {}
                Err(error) => panic!("{error:?}"),
            }
            match store.call(
                store
                    .wallet
                    .credit(account, &CreditId(String::from(id)), usd(amount)),
            ) {
                Ok(Credited::Applied) => {}
                other => panic!("{other:?}"),
            }
        }
        let request = RequestId(String::from("tamper-req"));
        assert!(store
            .call(store.wallet.hold(&first, &request, &key, None, usd(30)))
            .is_ok());
        assert_eq!(books(&store), Ok(()));

        tamper(
            &store,
            "UPDATE balances SET available = available - 10, held = held + 10 WHERE account = 't1'",
        );
        assert_eq!(
            books(&store),
            Err(billing_domain::InvariantViolation::AvailableMismatch {
                account: first.clone(),
                currency: Currency::Usd,
            })
        );
        tamper(
            &store,
            "UPDATE balances SET available = available + 10, held = held - 10 WHERE account = 't1'",
        );
        assert_eq!(books(&store), Ok(()));

        tamper(
            &store,
            "UPDATE balances SET held = held + 1, available = available - 1 WHERE account = 't2'",
        );
        assert_eq!(
            books(&store),
            Err(billing_domain::InvariantViolation::AvailableMismatch {
                account: second.clone(),
                currency: Currency::Usd,
            })
        );
        tamper(
            &store,
            "UPDATE balances SET held = held - 1, available = available + 1 WHERE account = 't2'",
        );

        tamper(
            &store,
            "UPDATE balances SET available = available + 1 WHERE account = 't2'",
        );
        assert!(matches!(
            books(&store),
            Err(billing_domain::InvariantViolation::NotConserved {
                currency: Currency::Usd
            })
        ));
        tamper(
            &store,
            "UPDATE balances SET available = available - 1 WHERE account = 't2'",
        );
        assert_eq!(books(&store), Ok(()));

        tamper(
            &store,
            "DELETE FROM entries WHERE reference = 'c-2' AND account_kind = 'funding'",
        );
        assert_eq!(
            books(&store),
            Err(billing_domain::InvariantViolation::Unbalanced {
                currency: Currency::Usd,
                debits: 130,
                credits: 180,
            })
        );
    }

    #[test]
    fn amount_above_i64_max_is_overflow() {
        let url = database_url();
        let drive = TokioDrive::start();
        let store = drive_open(&url, drive.runtime.handle());
        let account = AccountId(String::from("overflow"));
        let amount = Money {
            currency: Currency::Usd,
            minor_units: 9_223_372_036_854_775_808,
        };
        let result = store.call(store.wallet.deposit(&account, amount));
        assert!(matches!(
            result,
            Err(WalletError::Rejected(BillingRejection::Overflow))
        ));
        let balance = store.call(store.wallet.balance(&account, Currency::Usd));
        assert!(matches!(
            balance,
            Err(WalletError::Storage(PostgresError::MissingBalance))
        ));
    }

    #[test]
    fn spent_counts_settled_and_open_inside_one_transaction() {
        let url = database_url();
        let drive = TokioDrive::start();
        let store = drive_open(&url, drive.runtime.handle());
        let account = AccountId(String::from("spent-tx"));
        let key = KeyId(String::from("spent-key"));
        let settled = RequestId(String::from("spent-settled"));
        let open = RequestId(String::from("spent-open"));
        let usd = |minor_units| Money {
            currency: Currency::Usd,
            minor_units,
        };
        assert!(store.call(store.wallet.deposit(&account, usd(200))).is_ok());
        let settled_id =
            match store.call(store.wallet.hold(&account, &settled, &key, None, usd(60))) {
                Ok(id) => id,
                Err(error) => panic!("{error:?}"),
            };
        assert!(store
            .call(
                store
                    .wallet
                    .settle(&account, &settled, &settled_id, usd(60))
            )
            .is_ok());
        assert!(store
            .call(store.wallet.hold(&account, &open, &key, None, usd(40)))
            .is_ok());
        let spent = match store.call(store.wallet.spent(&key, Currency::Usd, UnixMillis(0))) {
            Ok(spent) => spent,
            Err(error) => panic!("{error:?}"),
        };
        assert_eq!(spent.minor_units, 100);
    }

    fn usd(minor_units: u64) -> Money {
        Money {
            currency: Currency::Usd,
            minor_units,
        }
    }

    fn exec(store: &PgContract, sql: &str) {
        let result = store
            .call(sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_owned())).execute(&store.wallet.pool));
        if let Err(error) = result {
            panic!("{error}");
        }
    }

    async fn waits_on(pool: &sqlx::PgPool, pid: i32) -> bool {
        for _ in 0..500 {
            let waiting: i64 = match sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))",
            )
            .bind(pid)
            .fetch_one(pool)
            .await
            {
                Ok(count) => count,
                Err(error) => panic!("{error}"),
            };
            if waiting > 0 {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        false
    }

    fn blocker(
        store: &PgContract,
        lock_sql: &str,
        bind: &str,
    ) -> (sqlx::Transaction<'static, sqlx::Postgres>, i32) {
        let mut tx = match store.call(store.wallet.pool.begin()) {
            Ok(tx) => tx,
            Err(error) => panic!("{error}"),
        };
        let pid: i32 =
            match store.call(sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&mut *tx)) {
                Ok(pid) => pid,
                Err(error) => panic!("{error}"),
            };
        if let Err(error) = store.call(
            sqlx::query(sqlx::AssertSqlSafe(lock_sql.to_owned()))
                .bind(bind.to_owned())
                .execute(&mut *tx),
        ) {
            panic!("{error}");
        }
        (tx, pid)
    }

    #[test]
    fn holds_on_one_key_wait_for_the_key_lock() {
        let url = database_url();
        let drive = TokioDrive::start();
        let store = drive_open(&url, drive.runtime.handle());
        let account = AccountId(String::from("key-lock"));
        let request = RequestId(String::from("key-lock-req"));
        let key = KeyId(format!("key-lock-{}", std::process::id()));
        assert!(store.call(store.wallet.deposit(&account, usd(100))).is_ok());
        let (tx, pid) = blocker(
            &store,
            "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
            key.0.as_str(),
        );
        let pool = store.wallet.pool.clone();
        let watcher = drive.runtime.spawn(async move {
            let waited = waits_on(&pool, pid).await;
            if let Err(error) = tx.commit().await {
                panic!("{error}");
            }
            waited
        });
        let result = store.call(store.wallet.hold(&account, &request, &key, None, usd(30)));
        let waited = match store.call(watcher) {
            Ok(waited) => waited,
            Err(error) => panic!("{error}"),
        };
        assert!(waited, "hold did not wait for the key lock");
        assert!(result.is_ok());
        let balance = store.call(store.wallet.balance(&account, Currency::Usd));
        assert!(matches!(balance, Ok(money) if money == usd(70)));
    }

    #[test]
    fn hold_losing_the_insert_race_on_its_request_is_a_duplicate() {
        let url = database_url();
        let drive = TokioDrive::start();
        let store = drive_open(&url, drive.runtime.handle());
        let account = AccountId(String::from("race-insert"));
        let request = RequestId(String::from("race-insert-req"));
        let key = KeyId(String::from("race-insert-late"));
        assert!(store.call(store.wallet.deposit(&account, usd(100))).is_ok());
        let (mut blocker, pid) = blocker(
            &store,
            "SELECT available FROM balances WHERE account = $1 AND currency = 'usd' FOR UPDATE",
            account.0.as_str(),
        );
        let pool = store.wallet.pool.clone();
        let first_account = account.0.clone();
        let first_request = request.0.clone();
        let first = drive.runtime.spawn(async move {
            assert!(
                waits_on(&pool, pid).await,
                "hold never waited on the balance row lock"
            );
            if let Err(error) = sqlx::query(
                "INSERT INTO holds (account, request, hold_key, currency, amount, state, created_at, hold_token) VALUES ($1, $2, 'race-insert-first', 'usd', 10, 'open', 0, 'race-insert-token')",
            )
            .bind(first_account)
            .bind(first_request)
            .execute(&mut *blocker)
            .await
            {
                panic!("{error}");
            }
            if let Err(error) = blocker.commit().await {
                panic!("{error}");
            }
        });
        let result = store.call(store.wallet.hold(&account, &request, &key, None, usd(30)));
        if let Err(error) = store.call(first) {
            panic!("{error}");
        }
        assert!(matches!(
            result,
            Err(WalletError::Rejected(BillingRejection::DuplicateRequest))
        ));
        let balance = store.call(store.wallet.balance(&account, Currency::Usd));
        assert!(matches!(balance, Ok(money) if money == usd(100)));
    }

    #[test]
    fn other_insert_failures_stay_storage_errors() {
        let url = database_url();
        let drive = TokioDrive::start();
        let store = drive_open(&url, drive.runtime.handle());
        let account = AccountId(String::from("insert-fails"));
        let request = RequestId(String::from("insert-fails-req"));
        let key = KeyId(String::from("blocked"));
        assert!(store.call(store.wallet.deposit(&account, usd(100))).is_ok());
        exec(
            &store,
            "ALTER TABLE holds ADD CONSTRAINT holds_reject_blocked CHECK (hold_key <> 'blocked')",
        );
        let result = store.call(store.wallet.hold(&account, &request, &key, None, usd(30)));
        assert!(matches!(
            result,
            Err(WalletError::Storage(PostgresError::Database(_)))
        ));
        let balance = store.call(store.wallet.balance(&account, Currency::Usd));
        assert!(matches!(balance, Ok(money) if money == usd(100)));
    }
}

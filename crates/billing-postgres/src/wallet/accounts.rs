use std::future::Future;

use billing::model::HoldId;
use billing::ports::Funding;
use billing_domain::{
    check_funding_currency, post_credit, CreditPosting, CreditRequest, Credited, FundingError,
    FundingRejection, FundingRow, Opened, PendingHold,
};
use kernel::model::{AccountId, CreditId, Currency, KeyId, Money, RequestId, UnixMillis};
use kernel::ports::Clock;
use sqlx::Row;

use super::{currency_code, database, insert_entries, money, parse_currency, PostgresWallet, Tx};
use crate::PostgresError;

impl<C: Clock + Sync> PostgresWallet<C> {
    /// 为 `(account, currency)` 建一行余额为 0 的记录，已存在就不动。
    ///
    /// # Errors
    ///
    /// 币种不是 USD 时返回 `FundingRejection::UnsupportedCurrency`。数据库失败时返回
    /// `FundingError::Storage(PostgresError::Database)`。
    pub async fn open_account(
        &self,
        account: &AccountId,
        currency: Currency,
    ) -> Result<Opened, FundingError<PostgresError>> {
        check_funding_currency(currency).map_err(FundingError::Rejected)?;
        let inserted = sqlx::query(
            "INSERT INTO balances (account, currency, available, held, deposited) VALUES ($1, $2, 0, 0, 0) ON CONFLICT DO NOTHING",
        )
        .bind(account.0.as_str())
        .bind(currency_code(currency))
        .execute(&self.pool)
        .await
        .map_err(credit_storage)?;
        if inserted.rows_affected() == 0 {
            Ok(Opened::AlreadyOpen)
        } else {
            Ok(Opened::Created)
        }
    }

    /// 管理员入账。同一账户的同一单号只记一次，余额和入账记录在一个事务里写入。
    ///
    /// # Errors
    ///
    /// 拒绝原因见 `billing_domain::post_credit`；结果超过 `i64::MAX` 时也返回
    /// `FundingRejection::Overflow`。钱包时钟超过 `i64::MAX` 时返回
    /// `FundingError::Storage(PostgresError::ClockOutOfRange)`。数据库失败或读到坏数据时返回
    /// `FundingError::Storage`。
    pub async fn credit(
        &self,
        account: &AccountId,
        credit: &CreditId,
        amount: Money,
    ) -> Result<Credited, FundingError<PostgresError>> {
        let mut tx = self.pool.begin().await.map_err(credit_storage)?;
        let credited = credit_in(&mut tx, self.clock.now(), account, credit, amount).await?;
        tx.commit().await.map_err(credit_storage)?;
        Ok(credited)
    }

    pub(super) async fn list_open_holds(
        &self,
        cutoff: UnixMillis,
    ) -> Result<Vec<PendingHold>, PostgresError> {
        let rows = sqlx::query(
            "SELECT hold_token, account, request, hold_key, currency, amount, created_at FROM holds WHERE state = 'open' AND created_at < $1 ORDER BY created_at, account COLLATE \"C\", request COLLATE \"C\"",
        )
        .bind(i64::try_from(cutoff.0).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(database)?;
        let mut pending = Vec::new();
        for row in rows {
            let currency: String = row.try_get("currency").map_err(database)?;
            pending.push(PendingHold {
                hold: HoldId(row.try_get("hold_token").map_err(database)?),
                account: AccountId(row.try_get("account").map_err(database)?),
                request: RequestId(row.try_get("request").map_err(database)?),
                key: KeyId(row.try_get("hold_key").map_err(database)?),
                amount: money(parse_currency(&currency)?, unsigned(&row, "amount")?),
                created_at: UnixMillis(unsigned(&row, "created_at")?),
            });
        }
        Ok(pending)
    }
}

impl<C: Clock + Sync> Funding for PostgresWallet<C> {
    type Error = PostgresError;

    fn open_account(
        &self,
        account: &AccountId,
        currency: Currency,
    ) -> impl Future<Output = Result<Opened, FundingError<Self::Error>>> + Send {
        PostgresWallet::open_account(self, account, currency)
    }

    fn credit(
        &self,
        account: &AccountId,
        credit: &CreditId,
        amount: Money,
    ) -> impl Future<Output = Result<Credited, FundingError<Self::Error>>> + Send {
        PostgresWallet::credit(self, account, credit, amount)
    }
}

async fn credit_in(
    tx: &mut Tx<'_>,
    now: UnixMillis,
    account: &AccountId,
    credit: &CreditId,
    amount: Money,
) -> Result<Credited, FundingError<PostgresError>> {
    let row = sqlx::query(
        "SELECT available, deposited FROM balances WHERE account = $1 AND currency = $2 FOR UPDATE",
    )
    .bind(account.0.as_str())
    .bind(currency_code(amount.currency))
    .fetch_optional(&mut **tx)
    .await
    .map_err(credit_storage)?;
    let funding = match row {
        Some(row) => Some(FundingRow {
            available: unsigned(&row, "available").map_err(FundingError::Storage)?,
            deposited: unsigned(&row, "deposited").map_err(FundingError::Storage)?,
        }),
        None => None,
    };
    let existing =
        sqlx::query("SELECT currency, amount FROM credits WHERE account = $1 AND credit_id = $2")
            .bind(account.0.as_str())
            .bind(credit.0.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(credit_storage)?;
    let existing = match existing {
        Some(row) => {
            let currency: String = row.try_get("currency").map_err(credit_storage)?;
            let currency = parse_currency(&currency).map_err(FundingError::Storage)?;
            Some(money(
                currency,
                unsigned(&row, "amount").map_err(FundingError::Storage)?,
            ))
        }
        None => None,
    };
    let applied = match post_credit(&CreditRequest {
        account,
        credit,
        amount,
        row: funding,
        existing,
        at: now,
    }) {
        Ok(CreditPosting::AlreadyApplied) => return Ok(Credited::AlreadyApplied),
        Ok(CreditPosting::Apply(applied)) => applied,
        Err(reason) => return Err(FundingError::Rejected(reason)),
    };
    let funded = applied.row;
    sqlx::query(
        "UPDATE balances SET available = $3, deposited = $4 WHERE account = $1 AND currency = $2",
    )
    .bind(account.0.as_str())
    .bind(currency_code(amount.currency))
    .bind(signed(funded.available)?)
    .bind(signed(funded.deposited)?)
    .execute(&mut **tx)
    .await
    .map_err(credit_storage)?;
    sqlx::query(
        "INSERT INTO credits (account, credit_id, currency, amount, at) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(account.0.as_str())
    .bind(credit.0.as_str())
    .bind(currency_code(amount.currency))
    .bind(signed(amount.minor_units)?)
    .bind(
        i64::try_from(now.0)
            .map_err(|_| FundingError::Storage(PostgresError::ClockOutOfRange))?,
    )
    .execute(&mut **tx)
    .await
    .map_err(credit_storage)?;
    insert_entries(tx, &applied.entries)
        .await
        .map_err(FundingError::Storage)?;
    Ok(Credited::Applied)
}

fn unsigned(row: &sqlx::postgres::PgRow, name: &str) -> Result<u64, PostgresError> {
    let value: i64 = row.try_get(name).map_err(database)?;
    u64::try_from(value).map_err(|_| PostgresError::AmountOutOfRange)
}

fn signed(value: u64) -> Result<i64, FundingError<PostgresError>> {
    i64::try_from(value).map_err(|_| FundingError::Rejected(FundingRejection::Overflow))
}

fn credit_storage(error: sqlx::Error) -> FundingError<PostgresError> {
    FundingError::Storage(database(error))
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::Arc;
    use std::time::Duration;

    use billing::errors::{BillingRejection, WalletError};
    use billing::ports::Wallet;
    use billing_domain::{CreditId, Credited, FundingError, FundingRejection, Opened};
    use billing_memory::ManualClock;
    use kernel::model::{AccountId, Currency, KeyId, Money, RequestId, UnixMillis};
    use sqlx::Row;

    use super::super::{migrations, run_migrations};
    use super::credit_in;
    use crate::testdb::{database_url, schema_pool};
    use crate::{PostgresError, PostgresWallet};

    type BackfilledEntry = (String, String, String, Option<String>, String, i64, i64);

    fn runtime() -> tokio::runtime::Runtime {
        match tokio::runtime::Builder::new_multi_thread()
            .enable_io()
            .enable_time()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => panic!("{error}"),
        }
    }

    fn block<T>(runtime: &tokio::runtime::Runtime, future: impl Future<Output = T>) -> T {
        runtime.block_on(future)
    }

    fn ok<T, E: std::fmt::Debug>(result: Result<T, E>) -> T {
        match result {
            Ok(value) => value,
            Err(error) => panic!("{error:?}"),
        }
    }

    fn usd(minor_units: u64) -> Money {
        Money {
            currency: Currency::Usd,
            minor_units,
        }
    }

    async fn wallet(now: u64) -> PostgresWallet<ManualClock> {
        let pool = schema_pool(&database_url()).await;
        ok(PostgresWallet::<ManualClock>::migrate(&pool).await);
        PostgresWallet::new(pool, ManualClock::new(UnixMillis(now)))
    }

    async fn execute(pool: &sqlx::PgPool, sql: &'static str) -> Result<(), sqlx::Error> {
        sqlx::query(sql).execute(pool).await.map(|_| ())
    }

    #[test]
    fn migration_backfills_created_at_from_the_first_held_entry() {
        let runtime = runtime();
        block(&runtime, async {
            let pool = schema_pool(&database_url()).await;
            let old: Vec<_> = migrations().into_iter().take(2).collect();
            ok(run_migrations(&pool, old).await);
            ok(execute(
                &pool,
                "INSERT INTO balances (account, currency, available, held, deposited) VALUES ('a', 'usd', 70, 10, 100), ('b', 'usd', 0, 0, 0)",
            )
            .await);
            ok(execute(
                &pool,
                "INSERT INTO holds (account, request, hold_key, currency, amount, state) VALUES ('a', 'r-open', 'k', 'usd', 10, 'open'), ('a', 'r-done', 'k', 'usd', 20, 'settled'), ('b', 'r-zero', 'k', 'usd', 0, 'open'), ('b', 'r-open', 'k', 'usd', 0, 'open')",
            )
            .await);
            ok(execute(
                &pool,
                "INSERT INTO entries (request, account_kind, account_id, entry_key, direction, currency, amount, at) VALUES ('r-open', 'available', 'a', 'k', 'debit', 'usd', 10, 500), ('r-open', 'held', 'a', 'k', 'credit', 'usd', 10, 500), ('r-open', 'held', 'a', 'k', 'debit', 'usd', 1, 900), ('r-done', 'held', 'a', 'k', 'credit', 'usd', 20, 300), ('r-done', 'held', 'a', 'k', 'debit', 'usd', 20, 400), ('r-done', 'available', 'a', 'k', 'debit', 'usd', 20, 300)",
            )
            .await);
            ok(run_migrations(&pool, migrations()).await);
            let rows = ok(sqlx::query(
                "SELECT account, request, created_at FROM holds ORDER BY account, request",
            )
            .fetch_all(&pool)
            .await);
            let created: Vec<(String, String, i64)> = rows
                .iter()
                .map(|row| {
                    (
                        ok(row.try_get("account")),
                        ok(row.try_get("request")),
                        ok(row.try_get("created_at")),
                    )
                })
                .collect();
            assert_eq!(
                created,
                [
                    (String::from("a"), String::from("r-done"), 300),
                    (String::from("a"), String::from("r-open"), 500),
                    (String::from("b"), String::from("r-open"), 0),
                    (String::from("b"), String::from("r-zero"), 0),
                ]
            );
            assert!(execute(
                &pool,
                "INSERT INTO holds (account, request, hold_key, currency, amount, state) VALUES ('a', 'r-new', 'k', 'usd', 1, 'open')",
            )
            .await
            .is_err());
            assert!(execute(
                &pool,
                "INSERT INTO holds (account, request, hold_key, currency, amount, state, created_at, hold_token) VALUES ('a', 'r-new', 'k', 'usd', 1, 'open', -1, 'legacy')",
            )
            .await
            .is_err());
            let wallet = PostgresWallet::new(pool, ManualClock::new(UnixMillis(0)));
            let pending = ok(wallet.open_holds_created_before(UnixMillis(u64::MAX)).await);
            let order: Vec<(&str, &str, u64)> = pending
                .iter()
                .map(|hold| {
                    (
                        hold.account.0.as_str(),
                        hold.request.0.as_str(),
                        hold.created_at.0,
                    )
                })
                .collect();
            assert_eq!(
                order,
                [("b", "r-open", 0), ("b", "r-zero", 0), ("a", "r-open", 500)]
            );
        });
    }

    #[test]
    fn migration_backfills_hold_tokens_for_existing_rows() {
        let runtime = runtime();
        block(&runtime, async {
            let pool = schema_pool(&database_url()).await;
            let old: Vec<_> = migrations().into_iter().take(5).collect();
            ok(run_migrations(&pool, old).await);
            let prior = "INSERT INTO holds (account, request, hold_key, currency, amount, state, created_at) VALUES ('a', 'done', 'k', 'usd', 10, 'settled', 1), ('a', 'open', 'k', 'usd', 10, 'open', 1)";
            ok(execute(&pool, prior).await);
            ok(run_migrations(&pool, migrations()).await);
            let rows = ok(sqlx::query("SELECT hold_token FROM holds")
                .fetch_all(&pool)
                .await);
            let mut tokens = Vec::new();
            for row in rows {
                tokens.push(ok(row.try_get::<String, _>("hold_token")));
            }
            assert_eq!(tokens.len(), 2);
            assert_ne!(tokens[0], tokens[1]);
            assert!(!tokens[0].is_empty() && !tokens[1].is_empty());
            let blank = "INSERT INTO holds (account, request, hold_key, currency, amount, state, created_at, hold_token) VALUES ('a', 'new', 'k', 'usd', 1, 'open', 0, '')";
            assert!(execute(&pool, blank).await.is_err());
        });
    }

    #[test]
    fn credits_table_rejects_bad_rows() {
        let runtime = runtime();
        block(&runtime, async {
            let wallet = wallet(0).await;
            ok(wallet
                .open_account(&AccountId(String::from("a")), Currency::Usd)
                .await);
            ok(execute(
                &wallet.pool,
                "INSERT INTO balances (account, currency, available, held, deposited) VALUES ('a', 'cny', 0, 0, 0)",
            )
            .await);
            for sql in [
                "INSERT INTO credits (account, credit_id, currency, amount, at) VALUES ('a', '', 'usd', 1, 0)",
                "INSERT INTO credits (account, credit_id, currency, amount, at) VALUES ('a', 'c', 'usd', 0, 0)",
                "INSERT INTO credits (account, credit_id, currency, amount, at) VALUES ('a', 'c', 'usd', 1, -1)",
                "INSERT INTO credits (account, credit_id, currency, amount, at) VALUES ('a', 'c', 'eur', 1, 0)",
                "INSERT INTO credits (account, credit_id, currency, amount, at) VALUES ('a', 'c', 'cny', 1, 0)",
                "INSERT INTO credits (account, credit_id, currency, amount, at) VALUES ('z', 'c', 'usd', 1, 0)",
            ] {
                assert!(execute(&wallet.pool, sql).await.is_err(), "{sql}");
            }
            ok(execute(
                &wallet.pool,
                "INSERT INTO credits (account, credit_id, currency, amount, at) VALUES ('a', 'c', 'usd', 1, 0)",
            )
            .await);
        });
    }

    #[test]
    fn credit_records_its_time_and_leaves_rows_unchanged_on_overflow() {
        let runtime = runtime();
        block(&runtime, async {
            let wallet = wallet(4_242).await;
            let account = AccountId(String::from("big"));
            assert_eq!(
                ok(wallet.open_account(&account, Currency::Usd).await),
                Opened::Created
            );
            assert!(matches!(
                wallet
                    .credit(&account, &CreditId(String::from("c-1")), usd(10))
                    .await,
                Ok(Credited::Applied)
            ));
            let at: i64 = ok(ok(sqlx::query(
                "SELECT at FROM credits WHERE account = 'big' AND credit_id = 'c-1'",
            )
            .fetch_one(&wallet.pool)
            .await)
            .try_get("at"));
            assert_eq!(at, 4_242);
            let too_big = u64::try_from(i64::MAX).map_or(u64::MAX, |max| max - 5);
            assert!(matches!(
                wallet
                    .credit(&account, &CreditId(String::from("c-2")), usd(too_big))
                    .await,
                Err(FundingError::Rejected(FundingRejection::Overflow))
            ));
            let past_i64 = u64::try_from(i64::MAX).map_or(u64::MAX, |max| max + 1);
            let fresh = AccountId(String::from("fresh"));
            ok(wallet.open_account(&fresh, Currency::Usd).await);
            assert!(matches!(
                wallet
                    .credit(&fresh, &CreditId(String::from("c-3")), usd(past_i64))
                    .await,
                Err(FundingError::Rejected(FundingRejection::Overflow))
            ));
            let row = ok(sqlx::query(
                "SELECT available, deposited FROM balances WHERE account = 'big'",
            )
            .fetch_one(&wallet.pool)
            .await);
            let available: i64 = ok(row.try_get("available"));
            let deposited: i64 = ok(row.try_get("deposited"));
            assert_eq!((available, deposited), (10, 10));
            let count: i64 = ok(ok(sqlx::query("SELECT COUNT(*) AS n FROM credits")
                .fetch_one(&wallet.pool)
                .await)
            .try_get("n"));
            assert_eq!(count, 1);
        });
    }

    #[test]
    fn a_failed_credit_insert_rolls_back_the_balance() {
        let runtime = runtime();
        block(&runtime, async {
            let wallet = wallet(0).await;
            let account = AccountId(String::from("blocked"));
            ok(wallet.open_account(&account, Currency::Usd).await);
            ok(execute(
                &wallet.pool,
                "ALTER TABLE credits ADD CONSTRAINT credits_reject_blocked CHECK (credit_id <> 'blocked')",
            )
            .await);
            assert!(matches!(
                wallet
                    .credit(&account, &CreditId(String::from("blocked")), usd(9))
                    .await,
                Err(FundingError::Storage(PostgresError::Database(_)))
            ));
            let row = ok(sqlx::query(
                "SELECT available, deposited FROM balances WHERE account = 'blocked'",
            )
            .fetch_one(&wallet.pool)
            .await);
            let available: i64 = ok(row.try_get("available"));
            let deposited: i64 = ok(row.try_get("deposited"));
            assert_eq!((available, deposited), (0, 0));
            assert_eq!(count_rows(&wallet.pool, "entries").await, 0);
        });
    }

    #[test]
    fn a_failed_entry_insert_rolls_back_the_credit() {
        let runtime = runtime();
        block(&runtime, async {
            let wallet = wallet(0).await;
            let account = AccountId(String::from("no-entry"));
            ok(wallet.open_account(&account, Currency::Usd).await);
            ok(execute(
                &wallet.pool,
                "ALTER TABLE entries ADD CONSTRAINT entries_reject_blocked CHECK (reference <> 'blocked')",
            )
            .await);
            assert!(matches!(
                wallet
                    .credit(&account, &CreditId(String::from("blocked")), usd(9))
                    .await,
                Err(FundingError::Storage(PostgresError::Database(_)))
            ));
            let row = ok(sqlx::query(
                "SELECT available, deposited FROM balances WHERE account = 'no-entry'",
            )
            .fetch_one(&wallet.pool)
            .await);
            let available: i64 = ok(row.try_get("available"));
            let deposited: i64 = ok(row.try_get("deposited"));
            assert_eq!((available, deposited), (0, 0));
            assert_eq!(count_rows(&wallet.pool, "credits").await, 0);
            assert_eq!(count_rows(&wallet.pool, "entries").await, 0);
            assert!(matches!(
                wallet
                    .credit(&account, &CreditId(String::from("fine")), usd(9))
                    .await,
                Ok(Credited::Applied)
            ));
            assert_eq!(count_rows(&wallet.pool, "credits").await, 1);
            assert_eq!(count_rows(&wallet.pool, "entries").await, 2);
        });
    }

    #[test]
    fn a_repeated_credit_never_adds_entries() {
        let runtime = runtime();
        block(&runtime, async {
            let wallet = wallet(11).await;
            let account = AccountId(String::from("once"));
            ok(wallet.open_account(&account, Currency::Usd).await);
            let id = CreditId(String::from("c-once"));
            for _ in 0..3 {
                ok(wallet.credit(&account, &id, usd(8)).await);
            }
            assert!(matches!(
                wallet.credit(&account, &id, usd(9)).await,
                Err(FundingError::Rejected(FundingRejection::ConflictingCredit))
            ));
            assert_eq!(count_rows(&wallet.pool, "credits").await, 1);
            let rows = ok(sqlx::query(
                "SELECT reference, reference_kind, account_kind, account_id, entry_key, direction, currency, amount, at FROM entries ORDER BY id",
            )
            .fetch_all(&wallet.pool)
            .await);
            let seen: Vec<(String, String, Option<String>, String, i64, i64)> = rows
                .iter()
                .map(|row| {
                    (
                        ok(row.try_get("reference_kind")),
                        ok(row.try_get("account_kind")),
                        ok(row.try_get("account_id")),
                        ok(row.try_get("direction")),
                        ok(row.try_get("amount")),
                        ok(row.try_get("at")),
                    )
                })
                .collect();
            assert_eq!(
                seen,
                [
                    (
                        String::from("credit"),
                        String::from("funding"),
                        None,
                        String::from("debit"),
                        8,
                        11
                    ),
                    (
                        String::from("credit"),
                        String::from("available"),
                        Some(String::from("once")),
                        String::from("credit"),
                        8,
                        11
                    ),
                ]
            );
            let references: Vec<String> = rows
                .iter()
                .map(|row| ok(row.try_get("reference")))
                .collect();
            assert_eq!(references, ["c-once", "c-once"]);
        });
    }

    #[test]
    fn entries_table_enforces_the_ledger_shape() {
        let runtime = runtime();
        block(&runtime, async {
            let wallet = wallet(0).await;
            for sql in [
                "INSERT INTO entries (reference, reference_kind, account_kind, account_id, entry_key, direction, currency, amount, at) VALUES ('c', 'credit', 'funding', 'a', NULL, 'debit', 'usd', 1, 0)",
                "INSERT INTO entries (reference, reference_kind, account_kind, account_id, entry_key, direction, currency, amount, at) VALUES ('c', 'credit', 'funding', NULL, NULL, 'credit', 'usd', 1, 0)",
                "INSERT INTO entries (reference, reference_kind, account_kind, account_id, entry_key, direction, currency, amount, at) VALUES ('c', 'request', 'funding', NULL, NULL, 'debit', 'usd', 1, 0)",
                "INSERT INTO entries (reference, reference_kind, account_kind, account_id, entry_key, direction, currency, amount, at) VALUES ('c', 'credit', 'available', 'a', 'k', 'credit', 'usd', 1, 0)",
                "INSERT INTO entries (reference, reference_kind, account_kind, account_id, entry_key, direction, currency, amount, at) VALUES ('c', 'credit', 'available', 'a', NULL, 'debit', 'usd', 1, 0)",
                "INSERT INTO entries (reference, reference_kind, account_kind, account_id, entry_key, direction, currency, amount, at) VALUES ('c', 'credit', 'held', 'a', NULL, 'credit', 'usd', 1, 0)",
                "INSERT INTO entries (reference, reference_kind, account_kind, account_id, entry_key, direction, currency, amount, at) VALUES ('c', 'credit', 'revenue', NULL, NULL, 'credit', 'usd', 1, 0)",
                "INSERT INTO entries (reference, reference_kind, account_kind, account_id, entry_key, direction, currency, amount, at) VALUES ('c', 'other', 'available', 'a', NULL, 'credit', 'usd', 1, 0)",
                "INSERT INTO entries (reference, reference_kind, account_kind, account_id, entry_key, direction, currency, amount, at) VALUES ('c', 'credit', 'bogus', NULL, NULL, 'credit', 'usd', 1, 0)",
                "INSERT INTO entries (reference, reference_kind, account_kind, account_id, entry_key, direction, currency, amount, at) VALUES ('c', 'request', 'available', NULL, NULL, 'credit', 'usd', 1, 0)",
            ] {
                assert!(execute(&wallet.pool, sql).await.is_err(), "{sql}");
            }
            ok(execute(
                &wallet.pool,
                "INSERT INTO entries (reference, reference_kind, account_kind, account_id, entry_key, direction, currency, amount, at) VALUES ('c', 'credit', 'funding', NULL, NULL, 'debit', 'usd', 1, 0), ('c', 'credit', 'available', 'a', NULL, 'credit', 'usd', 1, 0), ('c', 'credit', 'available', 'b', NULL, 'credit', 'usd', 1, 0), ('c', 'request', 'available', 'a', 'k', 'debit', 'usd', 1, 0)",
            )
            .await);
            assert!(execute(
                &wallet.pool,
                "INSERT INTO entries (reference, reference_kind, account_kind, account_id, entry_key, direction, currency, amount, at) VALUES ('c', 'credit', 'available', 'a', NULL, 'credit', 'usd', 1, 0)",
            )
            .await
            .is_err());
        });
    }

    #[test]
    fn migration_backfills_funding_entries_for_existing_credits() {
        let runtime = runtime();
        block(&runtime, async {
            let pool = schema_pool(&database_url()).await;
            let old: Vec<_> = migrations().into_iter().take(4).collect();
            ok(run_migrations(&pool, old).await);
            ok(execute(
                &pool,
                "INSERT INTO balances (account, currency, available, held, deposited) VALUES ('a', 'usd', 15, 0, 15), ('b', 'usd', 7, 0, 7)",
            )
            .await);
            ok(execute(
                &pool,
                "INSERT INTO credits (account, credit_id, currency, amount, at) VALUES ('b', 'c-1', 'usd', 7, 20), ('a', 'c-1', 'usd', 10, 20), ('a', 'c-0', 'usd', 5, 10)",
            )
            .await);
            ok(execute(
                &pool,
                "INSERT INTO entries (request, account_kind, account_id, entry_key, direction, currency, amount, at) VALUES ('r', 'held', 'a', 'k', 'credit', 'usd', 1, 5)",
            )
            .await);
            ok(run_migrations(&pool, migrations()).await);
            let rows = ok(sqlx::query(
                "SELECT reference, reference_kind, account_kind, account_id, direction, amount, at FROM entries ORDER BY id",
            )
            .fetch_all(&pool)
            .await);
            let seen: Vec<BackfilledEntry> = rows
                .iter()
                .map(|row| {
                    (
                        ok(row.try_get("reference")),
                        ok(row.try_get("reference_kind")),
                        ok(row.try_get("account_kind")),
                        ok(row.try_get("account_id")),
                        ok(row.try_get("direction")),
                        ok(row.try_get("amount")),
                        ok(row.try_get("at")),
                    )
                })
                .collect();
            let row = |reference: &str,
                       kind: &str,
                       account: &str,
                       id: Option<&str>,
                       direction: &str,
                       amount,
                       at| {
                (
                    String::from(reference),
                    String::from(kind),
                    String::from(account),
                    id.map(String::from),
                    String::from(direction),
                    amount,
                    at,
                )
            };
            assert_eq!(
                seen,
                [
                    row("r", "request", "held", Some("a"), "credit", 1, 5),
                    row("c-0", "credit", "funding", None, "debit", 5, 10),
                    row("c-0", "credit", "available", Some("a"), "credit", 5, 10),
                    row("c-1", "credit", "funding", None, "debit", 10, 20),
                    row("c-1", "credit", "available", Some("a"), "credit", 10, 20),
                    row("c-1", "credit", "funding", None, "debit", 7, 20),
                    row("c-1", "credit", "available", Some("b"), "credit", 7, 20),
                ]
            );
        });
    }

    async fn count_rows(pool: &sqlx::PgPool, table: &'static str) -> i64 {
        ok(
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
                .fetch_one(pool)
                .await,
        )
    }

    #[test]
    fn a_repeated_credit_waits_for_the_first_and_is_applied_once() {
        let runtime = runtime();
        let wallet = Arc::new(block(&runtime, wallet(0)));
        let account = AccountId(String::from("racer"));
        let credit = CreditId(String::from("c-race"));
        ok(block(
            &runtime,
            wallet.open_account(&account, Currency::Usd),
        ));
        let mut first = ok(block(&runtime, wallet.pool.begin()));
        let pid: i32 = ok(block(
            &runtime,
            sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&mut *first),
        ));
        assert!(matches!(
            block(
                &runtime,
                credit_in(&mut first, UnixMillis(0), &account, &credit, usd(5))
            ),
            Ok(Credited::Applied)
        ));
        let second = {
            let wallet = Arc::clone(&wallet);
            let account = account.clone();
            let credit = credit.clone();
            runtime.spawn(async move { wallet.credit(&account, &credit, usd(5)).await })
        };
        let mut waited = 0;
        while waited < 500 && !block(&runtime, blocks_someone(&wallet.pool, pid)) {
            std::thread::sleep(Duration::from_millis(10));
            waited += 1;
        }
        assert!(waited < 500);
        ok(block(&runtime, first.commit()));
        let second: Result<Credited, FundingError<PostgresError>> = ok(block(&runtime, second));
        assert!(matches!(second, Ok(Credited::AlreadyApplied)));
        let deposited: i64 = ok(ok(block(
            &runtime,
            sqlx::query("SELECT SUM(deposited)::BIGINT AS total FROM balances")
                .fetch_one(&wallet.pool),
        ))
        .try_get("total"));
        assert_eq!(deposited, 5);
    }

    #[test]
    fn migration_fails_when_a_funded_hold_has_no_held_entry() {
        let runtime = runtime();
        block(&runtime, async {
            let pool = schema_pool(&database_url()).await;
            let old: Vec<_> = migrations().into_iter().take(2).collect();
            ok(run_migrations(&pool, old).await);
            ok(execute(
                &pool,
                "INSERT INTO balances (account, currency, available, held, deposited) VALUES ('a', 'usd', 90, 10, 100)",
            )
            .await);
            ok(execute(
                &pool,
                "INSERT INTO holds (account, request, hold_key, currency, amount, state) VALUES ('a', 'r-lost', 'k', 'usd', 10, 'open'), ('a', 'r-zero', 'k', 'usd', 0, 'open')",
            )
            .await);
            assert!(matches!(
                run_migrations(&pool, migrations()).await,
                Err(PostgresError::Migrate(_))
            ));
            let applied: i64 = ok(sqlx::query_scalar(
                "SELECT MAX(version) FROM _sqlx_migrations WHERE success",
            )
            .fetch_one(&pool)
            .await);
            assert_eq!(applied, 2);
            assert!(execute(&pool, "SELECT created_at FROM holds")
                .await
                .is_err());
        });
    }

    #[test]
    fn a_clock_past_i64_is_a_storage_error() {
        let runtime = runtime();
        block(&runtime, async {
            let wallet = wallet(0).await;
            let late =
                PostgresWallet::new(wallet.pool.clone(), ManualClock::new(UnixMillis(u64::MAX)));
            let account = AccountId(String::from("late"));
            let key = KeyId(String::from("late-key"));
            let held = RequestId(String::from("late-held"));
            let fresh = RequestId(String::from("late-fresh"));
            ok(wallet.open_account(&account, Currency::Usd).await);
            assert!(matches!(
                late.credit(&account, &CreditId(String::from("c-late")), usd(10))
                    .await,
                Err(FundingError::Storage(PostgresError::ClockOutOfRange))
            ));
            assert!(matches!(
                wallet
                    .credit(&account, &CreditId(String::from("c-now")), usd(10))
                    .await,
                Ok(Credited::Applied)
            ));
            assert!(matches!(
                late.hold(&account, &fresh, &key, None, usd(3)).await,
                Err(WalletError::Storage(PostgresError::ClockOutOfRange))
            ));
            let held_id = ok(wallet.hold(&account, &held, &key, None, usd(4)).await);
            assert!(matches!(
                late.release(&account, &held, &held_id).await,
                Err(WalletError::Storage(PostgresError::ClockOutOfRange))
            ));
            assert_eq!(ok(wallet.held(&account, &held).await), usd(4));
            assert!(matches!(
                wallet.held(&account, &fresh).await,
                Err(WalletError::Rejected(BillingRejection::HoldNotFound))
            ));
            assert_eq!(ok(wallet.balance(&account, Currency::Usd).await), usd(6));
            let count: i64 = ok(sqlx::query_scalar("SELECT COUNT(*) FROM credits")
                .fetch_one(&wallet.pool)
                .await);
            assert_eq!(count, 1);
        });
    }

    async fn blocks_someone(pool: &sqlx::PgPool, pid: i32) -> bool {
        let waiting: Result<i64, sqlx::Error> = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))",
        )
        .bind(pid)
        .fetch_one(pool)
        .await;
        ok(waiting) > 0
    }
}

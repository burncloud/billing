use billing_domain::{ModelPrice, NanosPerMillionTokens, PriceBook};
use kernel::model::ModelId;
use sqlx::Row;

use crate::wallet::parse_currency;
use crate::PostgresError;

/// 从 `model_prices` 表读出整张价格表。表由管理员写，本类型只读。
pub struct PostgresPrices {
    pool: sqlx::PgPool,
}

impl PostgresPrices {
    #[must_use]
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }

    /// 每一行都重新经过 `ModelPrice::new` 校验，任何一行不合法整张表都读取失败，不跳过。
    ///
    /// # Errors
    ///
    /// 查询失败时返回 `PostgresError::Database`。币种无法识别时返回 `PostgresError::UnknownCurrency`。单价是负数时返回 `PostgresError::AmountOutOfRange`。`ModelPrice::new` 拒绝的行返回 `PostgresError::InvalidPrice`。模型重复时返回 `PostgresError::DuplicateModel`。
    pub async fn load(&self) -> Result<PriceBook, PostgresError> {
        let rows = sqlx::query(
            "SELECT model, currency, input_nanos_per_mtok, cached_input_nanos_per_mtok, output_nanos_per_mtok FROM model_prices ORDER BY model",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(database)?;
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let model: String = row.try_get("model").map_err(database)?;
            let currency: String = row.try_get("currency").map_err(database)?;
            let price = ModelPrice::new(
                parse_currency(&currency)?,
                rate(&row, "input_nanos_per_mtok")?,
                rate(&row, "cached_input_nanos_per_mtok")?,
                rate(&row, "output_nanos_per_mtok")?,
            )
            .map_err(|reason| PostgresError::InvalidPrice {
                model: model.clone(),
                reason,
            })?;
            entries.push((ModelId(model), price));
        }
        PriceBook::new(entries).map_err(|_| PostgresError::DuplicateModel)
    }
}

fn rate(row: &sqlx::postgres::PgRow, name: &str) -> Result<NanosPerMillionTokens, PostgresError> {
    let value: i64 = row.try_get(name).map_err(database)?;
    u64::try_from(value)
        .map(NanosPerMillionTokens::new)
        .map_err(|_| PostgresError::AmountOutOfRange)
}

fn database(error: sqlx::Error) -> PostgresError {
    PostgresError::Database(Box::new(error))
}

#[cfg(test)]
mod tests {
    use billing::model::{ModelUsage, TokenUsage};
    use billing::ports::Pricing;
    use billing_domain::{PriceLookupError, PriceRejection};
    use kernel::model::{Currency, Money};

    use super::PostgresPrices;
    use crate::testdb::{database_url, schema_pool};
    use crate::{PostgresError, PostgresWallet};

    struct Store {
        runtime: tokio::runtime::Runtime,
        pool: sqlx::PgPool,
    }

    impl Store {
        fn open() -> Self {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_io()
                .enable_time()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => panic!("{error}"),
            };
            let url = database_url();
            let pool = runtime.block_on(async {
                let pool = schema_pool(&url).await;
                if let Err(error) = PostgresWallet::<()>::migrate(&pool).await {
                    panic!("{error}");
                }
                pool
            });
            Self { runtime, pool }
        }

        fn exec(&self, sql: &str) -> Result<(), sqlx::Error> {
            self.runtime.block_on(async {
                sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_owned()))
                    .execute(&self.pool)
                    .await
                    .map(|_| ())
            })
        }

        fn insert(&self, values: &str) -> Result<(), sqlx::Error> {
            self.exec(&format!(
                "INSERT INTO model_prices (model, currency, input_nanos_per_mtok, cached_input_nanos_per_mtok, output_nanos_per_mtok, updated_at) VALUES {values}"
            ))
        }

        fn load(&self) -> Result<billing_domain::PriceBook, PostgresError> {
            self.runtime
                .block_on(PostgresPrices::new(self.pool.clone()).load())
        }

        fn drop_checks(&self) {
            if let Err(error) = self.exec(
                "DO $$ DECLARE c record; BEGIN FOR c IN SELECT conname FROM pg_constraint WHERE conrelid = 'model_prices'::regclass AND contype = 'c' LOOP EXECUTE format('ALTER TABLE model_prices DROP CONSTRAINT %I', c.conname); END LOOP; END $$",
            ) {
                panic!("{error}");
            }
        }
    }

    fn usage(model: &str, input: u64, cached: u64, output: u64) -> ModelUsage {
        ModelUsage {
            model: kernel::model::ModelId(String::from(model)),
            tokens: TokenUsage {
                input_tokens: input,
                cached_input_tokens: cached,
                output_tokens: output,
            },
        }
    }

    fn usd(minor_units: u64) -> Money {
        Money {
            currency: Currency::Usd,
            minor_units,
        }
    }

    #[test]
    fn stored_prices_are_read_back_and_priced() {
        let store = Store::open();
        assert!(store
            .insert("('gpt-4o-mini', 'usd', 150000000, 75000000, 600000000, 0), ('gpt-4o', 'usd', 2500000000, 1250000000, 10000000000, 0)")
            .is_ok());
        let book = match store.load() {
            Ok(book) => book,
            Err(error) => panic!("{error}"),
        };
        assert_eq!(
            book.price(&usage("gpt-4o-mini", 19, 0, 816)),
            Ok(usd(492_450))
        );
        assert_eq!(
            book.price(&usage("gpt-4o-mini", 40, 60, 0)),
            Ok(usd(10_500))
        );
        assert_eq!(book.price(&usage("gpt-4o", 1, 0, 1)), Ok(usd(12_500)));
    }

    #[test]
    fn lookup_is_exact() {
        let store = Store::open();
        assert!(store
            .insert("('gpt-4o', 'usd', 2500000000, 1250000000, 10000000000, 0)")
            .is_ok());
        let book = match store.load() {
            Ok(book) => book,
            Err(error) => panic!("{error}"),
        };
        for model in ["gpt-4o-mini", "GPT-4o", "gpt-4o-2024-11-20", "unknown"] {
            assert_eq!(
                book.price(&usage(model, 1, 0, 1)),
                Err(PriceLookupError::UnknownModel)
            );
        }
    }

    #[test]
    fn empty_table_prices_nothing() {
        let store = Store::open();
        let book = match store.load() {
            Ok(book) => book,
            Err(error) => panic!("{error}"),
        };
        assert_eq!(
            book.price(&usage("gpt-4o", 1, 0, 1)),
            Err(PriceLookupError::UnknownModel)
        );
    }

    #[test]
    fn checks_reject_invalid_rows() {
        let store = Store::open();
        for values in [
            "('zero-input', 'usd', 0, 1, 1, 0)",
            "('zero-cached', 'usd', 1, 0, 1, 0)",
            "('zero-output', 'usd', 1, 1, 0, 0)",
            "('negative', 'usd', -1, 1, 1, 0)",
            "('cached-above', 'usd', 1, 2, 1, 0)",
            "('eur', 'eur', 1, 1, 1, 0)",
            "('', 'usd', 1, 1, 1, 0)",
            "('negative-time', 'usd', 1, 1, 1, -1)",
        ] {
            assert!(store.insert(values).is_err(), "{values}");
        }
        assert!(store.insert("('ok', 'usd', 1, 1, 1, 0)").is_ok());
        assert!(store.insert("('ok', 'usd', 2, 1, 2, 0)").is_err());
    }

    #[test]
    fn cny_row_fails_the_whole_load() {
        let store = Store::open();
        assert!(store
            .insert("('ok', 'usd', 1, 1, 1, 0), ('cny-model', 'cny', 1, 1, 1, 0)")
            .is_ok());
        assert!(matches!(
            store.load(),
            Err(PostgresError::InvalidPrice { model, reason: PriceRejection::UnsupportedCurrency }) if model == "cny-model"
        ));
    }

    #[test]
    fn rows_past_the_checks_fail_the_load() {
        let cases = [
            (
                "('zero', 'usd', 0, 0, 1, 0)",
                Some(PriceRejection::ZeroRate),
            ),
            (
                "('cached-above', 'usd', 1, 2, 1, 0)",
                Some(PriceRejection::CachedInputAboveInput),
            ),
            ("('negative', 'usd', -1, 1, 1, 0)", None),
        ];
        for (values, expected) in cases {
            let store = Store::open();
            store.drop_checks();
            assert!(store.insert(values).is_ok(), "{values}");
            let result = store.load();
            match expected {
                Some(reason) => assert!(
                    matches!(&result, Err(PostgresError::InvalidPrice { reason: got, .. }) if *got == reason),
                    "{values}: {result:?}"
                ),
                None => assert!(
                    matches!(result, Err(PostgresError::AmountOutOfRange)),
                    "{values}: {result:?}"
                ),
            }
        }
    }

    #[test]
    fn unknown_currency_fails_the_load() {
        let store = Store::open();
        store.drop_checks();
        assert!(store.insert("('eur', 'eur', 1, 1, 1, 0)").is_ok());
        assert!(matches!(store.load(), Err(PostgresError::UnknownCurrency)));
    }
}

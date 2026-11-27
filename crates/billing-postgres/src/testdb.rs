use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::postgres::PgPoolOptions;

pub(crate) fn database_url() -> String {
    match std::env::var("DATABASE_URL") {
        Ok(url) if !url.is_empty() => url,
        _ => panic!("DATABASE_URL is required"),
    }
}

fn next_schema() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "b{}_{:x}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

pub(crate) async fn schema_pool(url: &str) -> sqlx::PgPool {
    let schema = next_schema();
    let admin = match sqlx::PgPool::connect(url).await {
        Ok(pool) => pool,
        Err(error) => panic!("{error}"),
    };
    if let Err(error) = sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
    {
        panic!("{error}");
    }
    admin.close().await;
    match PgPoolOptions::new()
        .max_connections(8)
        .after_connect(move |conn, _meta| {
            let schema_name = schema.clone();
            Box::pin(async move {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "SET search_path TO {schema_name}"
                )))
                .execute(&mut *conn)
                .await?;
                Ok(())
            })
        })
        .connect(url)
        .await
    {
        Ok(pool) => pool,
        Err(error) => panic!("{error}"),
    }
}

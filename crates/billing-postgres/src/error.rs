/// `Wallet::Error`，也是 `PostgresPrices::load` 的错误。包住数据库失败、迁移失败，以及读到的坏数据。
#[derive(Debug, thiserror::Error)]
pub enum PostgresError {
    #[error("没有余额行")]
    MissingBalance,
    #[error("余额行上的冻结额小于未完成冻结")]
    HeldBelowOpen,
    #[error("数据库错误")]
    Database(#[source] Box<sqlx::Error>),
    #[error("迁移失败")]
    Migrate(#[source] Box<sqlx::migrate::MigrateError>),
    #[error("金额超出已校验范围")]
    AmountOutOfRange,
    #[error("币种无法识别")]
    UnknownCurrency,
    #[error("冻结状态无法识别")]
    UnknownHold,
    #[error("分录无法识别")]
    UnknownEntry,
    #[error("价格行不合法：{model}")]
    InvalidPrice {
        model: String,
        reason: billing_domain::PriceRejection,
    },
    #[error("价格表里有重复模型")]
    DuplicateModel,
}

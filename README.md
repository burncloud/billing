# billing

BurnCloud 计费实现仓库。契约在 [burncloud/next](https://github.com/burncloud/next)，rev 钉在 `crates/billing-domain/Cargo.toml`。

`crates/billing-domain` 是纯逻辑：冻结、结算、释放、账本不变式、按模型单价计价（`ModelPrice`），以及按模型名精确查价的价格表快照（`PriceBook`，实现 `Pricing<Usage = ModelUsage>`）。它不访问数据库。余额行不存在时返回 `WalletError::Storage`。`Revenue` 按币种分开记。对 next 的依赖版本钉 `=0.3.0`。IO 依赖由 `crates/billing-domain/bans.toml` 禁止，`cargo test` 会核对清单。`crates/billing-memory` 只用于测试，不作生产实现。`crates/billing-postgres` 是 Postgres 钱包，并用 `PostgresPrices::load` 从 `model_prices` 表读出 `PriceBook`。它的测试需要 `DATABASE_URL`，没有就失败。

开户和入账只在 `PostgresWallet` 上提供，next 的 `Wallet` 契约不变。`open_account` 为 `(账户, 币种)` 建一行余额为 0 的记录，已存在就返回 `Opened::AlreadyOpen`。`credit` 按 `(账户, 入账单号)` 去重：同单号同金额同币种返回 `Credited::AlreadyApplied`，金额或币种不同返回 `CreditRejection::ConflictingCredit`。入账在一个事务里把 `available` 和 `deposited` 加同一个数，并在 `credits` 表记一行，所以 `check_ledger` 的守恒式不变。next 现在没有资金来源一类的 `LedgerAccount`，入账不写 `entries`。没开户的账户冻结时仍返回 `WalletError::Storage(PostgresError::MissingBalance)`，不当作余额不足。`holds.created_at` 由钱包时钟写入，`open_holds_created_before` 按它列出未完成的冻结；迁移 `0003` 用每笔冻结最早一条 `held` 分录的时间回填旧行，没有分录的回填 0。

根清单是 virtual workspace，`[workspace.lints.rust] unsafe_code = "forbid"` 与原来的 `[lints.rust]` 是同一条规则。成员只写 `[lints] workspace = true`。新建 crate 的这张 lints 表相对空基线有变化，`protected-paths` 会失败，要由 Code Owner 人工审。

`.github/workflows/ci.yml` 调用 [burncloud/ci](https://github.com/burncloud/ci) 的共享工作流，`uses` 钉 `@latest`，并打开 `postgres: true`。

## 本地复现

需要 `rust-toolchain.toml` 里的 `1.100.0`（含 `clippy` 和 `rustfmt`）。

```bash
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings -W clippy::pedantic -D clippy::allow_attributes_without_reason
cargo test --locked --workspace --all-targets --all-features
```

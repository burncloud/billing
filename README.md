# billing

BurnCloud 计费实现仓库。契约在 [burncloud/next](https://github.com/burncloud/next)，rev 钉在 `crates/billing-domain/Cargo.toml`。

`crates/billing-domain` 是纯逻辑：冻结、结算、释放、账本不变式、按模型单价计价（`ModelPrice`），以及按模型名精确查价的价格表快照（`PriceBook`，实现 `Pricing<Usage = ModelUsage>`）。它不访问数据库。余额行不存在时返回 `WalletError::Storage`。`Revenue` 按币种分开记。对 next 的依赖版本钉 `=0.5.0`。IO 依赖由 `crates/billing-domain/bans.toml` 禁止，`cargo test` 会核对清单。`crates/billing-memory` 只用于测试，不作生产实现。`crates/billing-postgres` 是 Postgres 钱包，并用 `PostgresPrices::load` 从 `model_prices` 表读出 `PriceBook`。它的测试需要 `DATABASE_URL`，没有就失败。

开户和入账只在 `PostgresWallet` 上提供，next 的 `Wallet` 契约不变。首期只收 USD：`open_account` 和 `credit` 遇到其他币种返回 `FundingRejection::UnsupportedCurrency`，`credits` 表也只收 `usd`。`open_account` 为 `(账户, USD)` 建一行余额为 0 的记录，已存在就返回 `Opened::AlreadyOpen`。`credit` 按 `(账户, 入账单号)` 去重：同单号同金额返回 `Credited::AlreadyApplied`，金额不同返回 `FundingRejection::ConflictingCredit`。入账在一个事务里把 `available` 和 `deposited` 加同一个数，在 `credits` 表记一行，并在 `entries` 写两条分录：借 `Funding`、贷 `Available(账户)`，`reference` 是 `EntryRef::Credit(入账单号)`；重复入账不记分录。`check_ledger` 从 `Funding` 借方推导充值总额，守恒式为 `available + held == Funding 借方合计 - revenue`，并逐账户核对 `Available`、`Held` 分录净额和钱包表里的 `available`、`held`。钱包时钟超过 `i64::MAX` 时，入账、冻结、结算、释放返回 `PostgresError::ClockOutOfRange`。`hold` 成功时返回 `HoldId`，并和这笔冻结一起保存。Postgres 在插入时写入 `gen_random_uuid()`，列是 `holds.hold_token`；迁移 `0006` 先给已有行回填随机值，再把列设成 `NOT NULL`，空字符串会被拒绝。内存钱包只用于测试，令牌是该钱包里从 1 起的十进制数。`settle` 和 `release` 只在冻结仍是 `open` 时比较令牌：没有记录、已经结算、已经释放保持原来的拒绝，不比较令牌；令牌对不上返回 `HoldMismatch`，冻结和余额不动。`hold` 失败不返回令牌，也不留下冻结。没开户的账户冻结时仍返回 `WalletError::Storage(PostgresError::MissingBalance)`，不当作余额不足。`holds.created_at` 由钱包时钟写入，`open_holds_created_before` 按它列出未完成的冻结；迁移 `0003` 用每笔冻结最早一条 `held` 分录的时间回填旧行，金额为 0 的冻结没有分录，回填 0；金额大于 0 却没有 `held` 分录的冻结会让迁移失败。

根清单是 virtual workspace，`[workspace.lints.rust] unsafe_code = "forbid"` 与原来的 `[lints.rust]` 是同一条规则。成员只写 `[lints] workspace = true`。新建 crate 的这张 lints 表相对空基线有变化，`protected-paths` 会失败，要由 Code Owner 人工审。

`.github/workflows/ci.yml` 调用 [burncloud/ci](https://github.com/burncloud/ci) 的共享工作流，`uses` 钉 `@latest`，并打开 `postgres: true`。

## 本地复现

需要 `rust-toolchain.toml` 里的 `1.100.0`（含 `clippy` 和 `rustfmt`）。

```bash
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings -W clippy::pedantic -D clippy::allow_attributes_without_reason
cargo test --locked --workspace --all-targets --all-features
```

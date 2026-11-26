# billing

BurnCloud 计费实现仓库。契约在 [burncloud/next](https://github.com/burncloud/next)，rev 钉在 `crates/billing-domain/Cargo.toml`。

`crates/billing-domain` 是纯逻辑：冻结、结算、释放和账本不变式。它不访问数据库。余额行不存在时返回 `WalletError::Storage`。`Revenue` 按币种分开记。依赖版本钉 `=0.1.0`。IO 依赖由 `crates/billing-domain/bans.toml` 禁止，`cargo test` 会核对清单。`crates/billing-memory` 只用于测试，不作生产实现。

根清单是 virtual workspace，`[workspace.lints.rust] unsafe_code = "forbid"` 与原来的 `[lints.rust]` 是同一条规则。成员只写 `[lints] workspace = true`。新建 crate 的这张 lints 表相对空基线有变化，`protected-paths` 会失败，要由 Code Owner 人工审。

`.github/workflows/ci.yml` 调用 [burncloud/ci](https://github.com/burncloud/ci) 的共享工作流，`uses` 钉 `@latest`。

## 本地复现

需要 `rust-toolchain.toml` 里的 `1.100.0`（含 `clippy` 和 `rustfmt`）。

```bash
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings -W clippy::pedantic -D clippy::allow_attributes_without_reason
cargo test --locked --workspace --all-targets --all-features
```

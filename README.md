# billing

BurnCloud 计费实现仓库。契约在 [burncloud/next](https://github.com/burncloud/next) 的 `bacb93048611f99b9b8a25ed2a04e3d54694cf04`。

`crates/billing-domain` 是纯逻辑：冻结、结算、释放和账本不变式。它不访问数据库。余额行不存在时返回 `WalletError::Storage`，不创建行。`Revenue` 按币种分开记。IO 依赖由 `crates/billing-domain/bans.toml` 禁止，`cargo test` 会核对清单。

`.github/workflows/ci.yml` 调用 [burncloud/ci](https://github.com/burncloud/ci) 的共享工作流，`uses` 钉 `@latest`。

## 本地复现

需要 `rust-toolchain.toml` 里的 `1.100.0`（含 `clippy` 和 `rustfmt`）。

```bash
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings -W clippy::pedantic -D clippy::allow_attributes_without_reason
cargo test --locked --workspace --all-targets --all-features
```

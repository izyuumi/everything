# Validation

Required Rust checks:

```sh
cargo fmt --all --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --release --locked
```

No new automated tests or test fixtures are created during this stage, including where an issue asks for them. The consolidated testing follow-up tracks all deferred assertions. Compilation and static analysis are not substitutes for those assertions.

Manual runtime verification must use disposable data and describe exactly what ran. Do not call paid model APIs, disclose workspace data externally, or mutate production resources merely for verification. Report unavailable model credentials, Apple SDKs, hardware benchmarks, and security review without claiming success.

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

The optional local embedding backend also requires all-feature workspace check, Clippy and release compilation. Linux/macOS CI runs both default and all-feature builds. A separate public-corpus macOS embedding workflow records real native inference and process RSS; it contains no model credentials or user data. The declared minimum Rust version is checked independently. Benchmark outcomes and deferred numerical verification remain distinct.

The optional native Apple clients use `.github/workflows/apple-validation.yml` to build and analyze the existing EverythingMac and EverythingIOS schemes on a macOS runner with signing disabled. Its scope is SDK compilation and static analysis; it does not launch either app, exercise launchd, prove that closing the menu bar preserves a running Host, or verify an iPhone encrypted roundtrip. Fresh exact-head results must be recorded before integration. No automated tests or runtime simulation are added.

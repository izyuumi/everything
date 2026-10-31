# Validation

Normal POC Rust merge validation runs on Ubuntu:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

No new automated tests or test fixtures are created during this stage, including where an issue asks for them. The consolidated testing follow-up tracks all deferred assertions. Compilation and static analysis are not substitutes for those assertions.

Manual runtime verification must use disposable data and describe exactly what ran. Do not call paid model APIs, disclose workspace data externally, or mutate production resources merely for verification. Report unavailable model credentials, Apple SDKs, hardware benchmarks, and security review without claiming success.

Clippy covers workspace/all-target compilation for both default and all-feature configurations, including the optional local embedding backend; duplicate `cargo check` jobs and optimized builds do not run on normal PRs. The manually invoked Extended validation workflow supplies macOS Rust, Rust 1.88 MSRV and default/all-feature optimized builds. Its separate default-false `retain_runtime_artifact` input uploads the default Linux CLI after its release build and before the all-feature build can replace it. This input moved from Validate to Extended validation; Validate's manual dispatch now runs the same lightweight Rust checks as a PR. Artifact retention does not execute the CLI. The separate public-corpus macOS embedding benchmark is manual only and records real native inference and process RSS when explicitly dispatched. Benchmark outcomes and deferred numerical/security verification remain distinct. See [POC CI](poc-ci.md) for exact jobs and manual boundaries.

The optional native Apple clients use `.github/workflows/apple-validation.yml` to detect relevant Apple/shared-protocol changes on Ubuntu, then build and analyze the existing VorraMac and VorraIOS schemes on macOS with signing disabled when relevant. Manual dispatch requests both SDK jobs. The always-emitted Apple compatibility gate rejects failed detection, failed/cancelled relevant SDK jobs and missing results; unrelated changes explicitly skip SDK work. Its scope is compilation/static analysis, without app launch, launchd, Host lifetime or iPhone encrypted-roundtrip verification. Record fresh exact-head gate results before integration and actual SDK results when relevant. Workflow definitions do not themselves configure branch protection; verify the repository's required checks separately. No automated tests or runtime simulation are added.

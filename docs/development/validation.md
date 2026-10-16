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

## Apple applications

The [Apple workflow](../../.github/workflows/apple-validation.yml) declares separate macOS-runner jobs for the existing `EverythingMac` and `EverythingIOS` schemes. Each invokes `xcodebuild build analyze` in Debug against the shared project, using generic macOS or iOS Simulator destinations and disabled code signing. Jobs have a thirty-minute timeout, read-only repository permissions and no persisted checkout credentials. Apple source/project or workflow changes trigger these jobs; no tests, application launch, signing service or extra dependency is introduced. The [macOS](macos-client.md) and [iOS](ios-client.md) guides retain the corresponding local validation commands and runtime checks.

This environment has no Xcode or Apple SDK. Workflow YAML, shell syntax and existing project/scheme/settings coherence can be checked locally, but these checks do not verify an Apple build or analyzer run. Repository billing currently prevents GitHub Actions startup, so this configuration supplies no passing CI result yet. Apple SDK compilation, platform API/concurrency checking, CryptoKit/device interoperability, UI and lifecycle behavior remain unverified until their actual commands and runtime observations complete. Deferred automated assertions remain in [#32](https://github.com/izyuumi/everything/issues/32).

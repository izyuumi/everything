# POC integration CI

Normal pull requests and pushes to main run the lightweight Validate and Apple validation workflows. Branch pushes have no separate automatic workflow run. Both workflows also support explicit manual dispatch. Per-workflow/event/PR-or-ref concurrency cancels superseded runs without cancelling another PR, main or a separately selected manual run. Pushes to main also run the [Release and Release iOS](releases.md) workflows, which publish the CLI beta and upload to TestFlight only while `VORRA_RELEASES` is `enabled`, and gate nothing.

## Normal merge feedback

Validate preserves the `rust (ubuntu-latest)` context and runs only:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

Its `release-guards` job runs `scripts/check_version.sh` (version placeholder drift) and `scripts/external_testflight.sh --audit` ([releases](releases.md#drift-guard)). It is a new context, not yet a required check.

These Clippy forms compile and statically inspect the complete default and all-feature workspace/all-target graph. Normal PRs perform no redundant cargo check, optimized build, macOS Rust job, MSRV job or embedding benchmark. Existing tests remain in the tree and no new tests, fixtures, framework or test requirement is introduced.

Apple validation always emits `Apple compatibility`. A small Ubuntu job reads the actual event's validated commit identities and compares the full PR base...head range or full push before..after range. A new branch's zero before identity and manual dispatch request SDK validation. Git runs with external diff/text conversion disabled, NUL-delimited filenames and rename detection disabled, so a rename includes both its removed and added paths. Filenames are compared as bytes and are never interpolated into shell commands. Missing commits or invalid detection data fail the gate instead of silently skipping SDK work.

Relevant changes are:

- Any `apps/Apple/` path or `.github/workflows/apple-validation.yml`.
- Cargo.toml or Cargo.lock.
- Rust `src/protocol.rs`, `src/protocol/`, `src/ipc.rs`, `src/crypto.rs`, `src/credentials.rs`, `src/devices.rs`, `src/devices/`, `src/relay_protocol.rs` or `src/transport/`.
- Native Settings `src/host/settings.rs`, `src/host/settings/`, `src/host/credentials.rs`, `src/host/client.rs` or `src/main.rs`, which carry the native/owner/credential/pairing payloads and CLI framing consumed by Apple clients.

The existing `VorraMac build and analyze` and `VorraIOS build and analyze` jobs retain their names and real Xcode build/analyze commands. Both must succeed for relevant changes. For irrelevant changes those jobs skip, and the final gate explicitly reports that no SDK work was required. The final gate runs after failures or cancellation and accepts only successful detection plus either both relevant SDK successes or an irrelevant SDK skip. Missing, failed or cancelled relevant results cannot satisfy it. Superseded expensive jobs cancel; their small final gate may still finish recording that unsuccessful old run.

Configure repository rules to require `rust (ubuntu-latest)` and `Apple compatibility`, then verify the actual settings. Workflow source alone does not enable branch protection. Existing protected contexts must be reconciled through authorized settings rather than bypassed or removed implicitly. Record the current exact-head checks before integration; historical SDK runs do not cover new Swift sources.

At the 2026-10-16 16:05 UTC settings audit, main was unprotected, and an integration token's attempt to require both contexts failed with HTTP 403 (`Resource not accessible by integration`). On 2026-10-17 the repository owner's own authorized credentials configured branch protection for main: required status checks `rust (ubuntu-latest)` and `Apple compatibility`, `strict` false (an up-to-date branch is not required), no required reviews, no push restrictions, and administrator enforcement off. A follow-up read of `repos/izyuumi/everything/branches/main/protection/required_status_checks` returned exactly those two contexts. Normal merges now need both contexts to pass on the PR head; administrators can still bypass deliberately, so the coordinator continues to check actual current-head results before merging.

## Explicit expensive validation

The separate Extended validation workflow is manual only. Its `Extended Rust (ubuntu-latest)` and `Extended Rust (macos-latest)` jobs perform strict default/all-feature Clippy and both default/all-feature optimized workspace builds. `Rust 1.88 MSRV` checks the all-target/all-feature workspace with Rust 1.88. Their distinct names prevent a manual job from substituting for the normal formatting/Clippy context. This broader work remains available when platform, compiler or optimized-build verification is needed, without making it every PR's prerequisite.

For a default Linux executable, dispatch Extended validation with `retain_runtime_artifact` set to true. The input keeps its original name, defaults to false and moved from Validate to Extended validation. Its order is default release build, upload `vorra-linux-default-${{ github.sha }}` from `target/release/vorra`, then all-feature release build. The retained artifact is the default-feature CLI; retention is three days. macOS does not upload this artifact. No CLI, Host, provider or database command runs through artifact retention. Validate manual dispatch has no artifact-retention input and remains lightweight.

Manual help/version observation has its own default-false `inspect_cli_startup` input in Extended validation. It runs only for an explicit workflow_dispatch on Linux, after the default release build and optional artifact upload, before the all-feature build can replace that executable. It invokes public help/version directly with disposable HOME/TMPDIR and a two-minute step timeout. Artifact retention alone executes no CLI command. See [CLI startup](cli-startup.md) for source ordering and remaining runtime verification; no successful startup observation is established by adding the step. Earlier observations retain their original source provenance and limitations.

Local embedding benchmark is also manual only. Its existing pinned public model provisioning, optimized native benchmark, process RSS collection and public report publication remain available when explicitly dispatched; embedding-related PRs do not run it automatically. It does not access model credentials or user data.

## Evidence limits

Workflow parsing and static review do not establish successful GitHub execution. Record actual current-head normal CI separately after publication. Manual validation has its own selected ref, command exits and artifacts and must be reported separately when it actually runs. No compiler/static-analysis result, public benchmark or retained executable establishes complete runtime behavior, security review or production readiness. Automated regression/security coverage remains deferred under [#32](https://github.com/izyuumi/everything/issues/32).

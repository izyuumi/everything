# POC integration CI

Normal pull requests and pushes to main run the lightweight Validate and Apple validation workflows. Branch pushes have no separate automatic workflow run. Both workflows also support explicit manual dispatch. Per-workflow/event/PR-or-ref concurrency cancels superseded runs without cancelling another PR, main or a separately selected manual run.

## Normal merge feedback

Validate preserves the `rust (ubuntu-latest)` context and runs only:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

These Clippy forms compile and statically inspect the complete default and all-feature workspace/all-target graph. Normal PRs perform no redundant cargo check, optimized build, macOS Rust job, MSRV job or embedding benchmark. Existing tests remain in the tree and no new tests, fixtures, framework or test requirement is introduced.

Apple validation always emits `Apple compatibility`. A small Ubuntu job reads the actual event's validated commit identities and compares the full PR base...head range or full push before..after range. A new branch's zero before identity and manual dispatch request SDK validation. Git runs with external diff/text conversion disabled, NUL-delimited filenames and rename detection disabled, so a rename includes both its removed and added paths. Filenames are compared as bytes and are never interpolated into shell commands. Missing commits or invalid detection data fail the gate instead of silently skipping SDK work.

Relevant changes are:

- Any `apps/Apple/` path or `.github/workflows/apple-validation.yml`.
- Cargo.toml or Cargo.lock.
- Rust `src/protocol.rs`, `src/protocol/`, `src/ipc.rs`, `src/crypto.rs`, `src/credentials.rs`, `src/devices.rs`, `src/devices/`, `src/relay_protocol.rs` or `src/transport/`.
- Native Settings `src/host/settings.rs`, `src/host/settings/`, `src/host/credentials.rs`, `src/host/client.rs` or `src/main.rs`, which carry the native/owner/credential/pairing payloads and CLI framing consumed by Apple clients.

The existing `EverythingMac build and analyze` and `EverythingIOS build and analyze` jobs retain their names and real Xcode build/analyze commands. Both must succeed for relevant changes. For irrelevant changes those jobs skip, and the final gate explicitly reports that no SDK work was required. The final gate runs after failures or cancellation and accepts only successful detection plus either both relevant SDK successes or an irrelevant SDK skip. Missing, failed or cancelled relevant results cannot satisfy it. Superseded expensive jobs cancel; their small final gate may still finish recording that unsuccessful old run.

Configure repository rules to require `rust (ubuntu-latest)` and `Apple compatibility`, then verify the actual settings. Workflow source alone does not enable branch protection. Existing protected contexts must be reconciled through authorized settings rather than bypassed or removed implicitly. Record the current exact-head checks before integration; historical SDK runs do not cover new Swift sources.

At the 2026-10-16 16:05 UTC settings audit, main was unprotected, with no required contexts or rules. The coordinator's authorized attempt to require both contexts failed with HTTP 403, `Resource not accessible by integration`; the subsequent settings read confirmed no change. Server enforcement remains unconfigured and requires integration access with repository Administration write permission. The coordinator still gates merges on the actual current-head fast checks; this process is not server-enforced branch protection.

## Explicit expensive validation

The separate Extended validation workflow is manual only. Its `Extended Rust (ubuntu-latest)` and `Extended Rust (macos-latest)` jobs perform strict default/all-feature Clippy and both default/all-feature optimized workspace builds. `Rust 1.88 MSRV` checks the all-target/all-feature workspace with Rust 1.88. Their distinct names prevent a manual job from substituting for the normal formatting/Clippy context. This broader work remains available when platform, compiler or optimized-build verification is needed, without making it every PR's prerequisite.

For a default Linux executable, dispatch Extended validation with `retain_runtime_artifact` set to true. The input keeps its original name, defaults to false and moved from Validate to Extended validation. Its order is default release build, upload `everything-linux-default-${{ github.sha }}` from `target/release/everything`, then all-feature release build. The retained artifact is the default-feature CLI; retention is three days. macOS does not upload this artifact. No CLI, Host, provider or database command runs through artifact retention. Validate manual dispatch has no artifact-retention input and remains lightweight.

Manual help/version observation is separate pending work in [#151](https://github.com/izyuumi/everything/issues/151). No startup probe or its proposed input is implemented by this CI change. A future explicit startup-observation input belongs to Extended validation and must remain independent of artifact retention. Earlier failed or successful observations retain their original source provenance and limitations.

Local embedding benchmark is also manual only. Its existing pinned public model provisioning, optimized native benchmark, process RSS collection and public report publication remain available when explicitly dispatched; embedding-related PRs do not run it automatically. It does not access model credentials or user data.

## Evidence limits

Workflow parsing and static review do not establish successful GitHub execution. Record actual current-head normal CI separately after publication. Manual validation has its own selected ref, command exits and artifacts and must be reported separately when it actually runs. No compiler/static-analysis result, public benchmark or retained executable establishes complete runtime behavior, security review or production readiness. Automated regression/security coverage remains deferred under [#32](https://github.com/izyuumi/everything/issues/32).

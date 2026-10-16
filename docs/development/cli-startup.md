# CLI startup

The synchronous entrypoint runs the existing Clap parser before constructing a Tokio runtime. Public `--help`, `--version` and parser-error exits therefore do not depend on asynchronous worker or driver startup. Genuine commands create the same default multithread runtime with `enable_all` and execute the original cancellation/select/result body in private `run_cli`. The original command schemas, `execute`, `shutdown_signal`, cleanup, worker/environment defaults and error exit classification are unchanged.

A returned runtime-build I/O error is printed and returns failure. This does not catch internal runtime panics, change resource limits or retry startup. There is one runtime and the top-level future is passed directly to block_on without spawning or adding a Send requirement.

## Optional public command observation

The manual [Extended validation workflow](../../.github/workflows/extended-validation.yml) has an explicit `inspect_cli_startup` boolean, default false. It runs only for workflow_dispatch on Linux, after the default release build and optional artifact upload, before the all-feature build can overwrite the executable. Its two-minute step invokes only `target/release/everything --help` and `target/release/everything --version`, with task-specific disposable HOME/TMPDIR. Public output appears directly in the run log. It starts no Host, opens no application data and invokes no model/provider command. No assertion harness, test framework or fixture is added.

An explicit dispatch after the workflow is available on the default branch is:

```sh
gh workflow run extended-validation.yml --repo izyuumi/everything --ref <ref> -F inspect_cli_startup=true
```

`retain_runtime_artifact` remains an independent default-false input. It can be enabled separately or alongside startup inspection; artifact upload occurs first and alone executes no CLI command. See [artifact provenance](manual-runtime-artifacts.md) and [POC CI](poc-ci.md) for the separate compilation, upload and normal merge gates.

Record the actual manual run URL/ID/attempt, checkout commit/tree, Linux default release-build result, whether the startup step actually ran, its public command output and actual outcome. A failure of help stops the step before version, which must then be reported as unobserved. Compilation, an upload, a skipped step or workflow source inspection does not establish either command's success. These runner observations do not reproduce a separately bounded local artifact environment or verify any Settings/Apple/model behavior.

## Evidence limits

Source formatting and workflow syntax checks establish no successful build or execution. Current formatting and strict default/all-feature Clippy CI remain required. Explicit extended builds/MSRV and actual default Linux help/version are separate observations before resolving [#151](https://github.com/izyuumi/everything/issues/151); deferred automated regression coverage remains [#32](https://github.com/izyuumi/everything/issues/32).

The prior bounded help observation of public default artifact `0eedd1e` exited 101 with no stdout; its stderr was deliberately discarded. Its cause remains unknown. The source-order change addresses an independently established startup dependency and must not be presented as the proven cause or repair of that historical failure. No fresh help/version runtime result is claimed by this source increment, and no production/security readiness claim follows.

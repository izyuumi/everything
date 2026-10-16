# Manual runtime artifacts

The [Validate workflow](../../.github/workflows/validate.yml) can retain the default-feature Linux CLI from a manually dispatched run. The `retain_runtime_artifact` boolean defaults to `false`; push and pull-request runs never upload this artifact. Both operating-system jobs and all existing formatting, check, Clippy, release-build and Rust 1.88 validation commands still run.

On `ubuntu-latest`, an opted-in manual run uploads only `target/release/everything` immediately after the default-feature release build and before the all-feature release build can replace it. The artifact is named `everything-linux-default-<full commit SHA>`, uses compression level 6, expires after three days, and fails its upload step if the binary is absent. No library directory, workspace data, Host state, keys or model files are included. This is a compiled public-source artifact, not an Apple application or an all-feature executable.

## Request and identify a build

After this workflow is available on the repository's default branch, open **Actions → Validate → Run workflow**, choose the intended ref and enable `retain_runtime_artifact`. The equivalent explicit dispatch is:

```sh
gh workflow run validate.yml --repo izyuumi/everything --ref <ref> -F retain_runtime_artifact=true
```

Dispatching does not identify a successful build by itself. Locate the actual manual run in Actions, verify its ref and full commit SHA, and record its URL, run ID, attempt number and final conclusion. If several runs target the same commit, select the intended run and attempt explicitly; the artifact name alone does not distinguish them. The Linux upload happens before later validation finishes, so an artifact can exist even when the complete run fails. Inspect the Linux default release-build and upload steps separately from the remaining validation results.

For an identified run:

```sh
gh run view <run-id> --repo izyuumi/everything --json headSha,event,status,conclusion,url,attempt
gh run download <run-id> --repo izyuumi/everything --name everything-linux-default-<full-commit-sha> --dir <new-artifact-directory>
git rev-parse '<full-commit-sha>^{tree}'
sha256sum <new-artifact-directory>/everything
```

Use a new disposable download directory. Record the commit and tree from that exact run, the downloaded binary's SHA-256 and size, the artifact ID and digest reported by the upload step, and the runner/toolchain details from the build log. The uploaded archive's digest and the executable's SHA-256 identify different bytes; retain both with their meanings. Do not substitute the current branch tip, a local build, an all-feature binary or a different run's artifact when attributing observations. Retain provenance before the three-day artifact expires. This workflow does not promise reproducible binary hashes across runner or toolchain changes.

## Separate manual verification

Artifact retention runs no CLI command, Host, model, helper or walkthrough. It adds no automated runtime verification. No upload or runtime result is established merely by this workflow change.

Any execution remains a separate, explicitly scoped manual action under the [validation policy](validation.md): use disposable state, bounded resources and the actual downloaded binary's identity, and record commands, outcomes, cleanup and unavailable platform or provider behavior. A successful compilation or upload does not establish Settings integration, OS credential storage, Apple UI behavior, inference, security review or the walkthrough acceptance in [#118](https://github.com/izyuumi/everything/issues/118). Deferred automated coverage remains tracked in [#32](https://github.com/izyuumi/everything/issues/32).

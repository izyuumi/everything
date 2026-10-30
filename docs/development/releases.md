# CLI releases

How the `everything` binary is versioned, built, published as a GitHub Releases beta and promoted to stable. The scheme is ported from izyuumi/yorozu's Mac release flow. Only the `everything` binary ships; the relay and the optional embedding binaries do not.

## Version

- `scripts/version.sh` prints the next version, by SemVer from the Conventional Commits since the last stable tag `v<major.minor.patch>` (prerelease tags such as `v0.2.0-beta` are ignored): a breaking change (`type!:` or a `BREAKING CHANGE:` footer) bumps major, or minor while below 1.0; a `feat` bumps minor; anything else patch. No file holds the version and nobody picks it. The scripts need the tags (`fetch-depth: 0` in CI). `VERSION=<x.y.z>` overrides it.
- The build number restarts at 1 per version: the current `v<version>-beta` release's `candidate.json` build plus one, or 1 (`scripts/release.sh --next`).
- The release build sets `EVERYTHING_VERSION=<version>+<build>` at compile time, so `everything --version` prints for example `everything 0.1.0+3`. Build metadata does not affect SemVer precedence. Without that variable (ordinary `cargo build`) it prints the `Cargo.toml` package version, which stays a fixed placeholder (`0.1.0`) and is never bumped by the release flow. Other places that report a version (`everything tui` title, MCP client info, HTTP user agent, local transport) still use the `Cargo.toml` version.

On this repository's history before any tag, `scripts/version.sh` prints `0.1.0`: no tag, `feat` commits and no breaking marker.

## Workflows

| Workflow | Trigger | Does |
| --- | --- | --- |
| `release.yml` | push to `main` except a change to only `CHANGELOG.md`; manual | `next`: `scripts/release.sh --next`. `build`: `scripts/release_build.sh <target>` on each target, signing macOS binaries when the secrets exist. `publish`: `scripts/release.sh --publish`, which replaces the beta and opens or updates the release PR. |
| `promote.yml` | the release PR (branch `release`, same repository) merged | `scripts/release.sh --promote <build> <sha> <digest>` with the PR body's `Release-Build:`, `Release-Commit:` and `Release-Digest:` lines. |

Both share the `release` concurrency group without cancellation, so a promote and the next beta never interleave. Both run only in `izyuumi/everything` and only while the repository variable `EVERYTHING_RELEASES` is `enabled`; otherwise every job is skipped. They run no tests; the normal [POC CI](poc-ci.md) gates merges.

Targets and runners: `aarch64-apple-darwin` and `x86_64-apple-darwin` on `macos-latest` (x86_64 cross-compiled), `x86_64-unknown-linux-gnu` on `ubuntu-22.04` and `aarch64-unknown-linux-gnu` on `ubuntu-22.04-arm`. The Linux binaries need glibc 2.35 or newer.

## Beta

Every push to `main` republishes the single prerelease `v<version>-beta`: `--publish` refuses a dirty tree, deletes the current beta release and its tag, then creates `v<version>-beta` at the pushed commit, not marked latest, with the assets below and notes from `scripts/release_notes.sh` (feat, fix and perf Conventional Commits since the last stable tag, dependency bumps left out, PR numbers kept). There is one beta at a time; when a `feat` lands the next beta moves to the new minor version.

## Stable

Stable releases work like release-please. After each beta, `scripts/release_pr.sh` resets the branch `release` to the beta's commit, commits `CHANGELOG.md` with the version's notes through the contents API (GitHub signs that commit), opens or updates the PR `chore: release <version>`, whose body ends with `Release-Build: <build>`, `Release-Commit: <source sha>` and `Release-Digest: sha256:<SHA256SUMS digest>`, and dispatches `validate.yml` and `apple-validation.yml` on `release` so the required checks report (a PR opened with the Actions token starts no `pull_request` workflows).

Check that beta, then merge the PR. `--promote` then republishes the beta's assets unchanged as `v<version>`, marked latest, and deletes the beta release and tag. Before publishing anything it checks that `candidate.json`'s build, tag, version and source commit, the release's target commit, the commit the beta tag names, the `SHA256SUMS` digest on GitHub and as downloaded, and every tarball against `SHA256SUMS` all match what the PR named; every mismatch is reported and it exits 1 without publishing or deleting anything. A newer beta updates the PR to name it. It does nothing if `v<version>` already exists. A missing or malformed argument exits 64.

Merging the release PR changes only `CHANGELOG.md`, so it starts no new beta; the next push does.

## Asset contract

Each release, beta or stable, carries exactly these assets. Names never contain the version, so a stable install can use `releases/latest/download/<asset>`.

| Asset | Content |
| --- | --- |
| `everything-<target>.tar.gz` | gzip tar holding one file, `everything`, at the archive root, mode 755. `<target>` is one of `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`. |
| `SHA256SUMS` | `shasum -a 256` lines (`<hex>  <asset>`) for every tarball. |
| `candidate.json` | `{"schema":1,"tag","version","build" (integer),"source_branch":"main","source_sha","assets":[{"target","name"}]}`. |

Tags: stable `v<major.minor.patch>`; beta `v<major.minor.patch>-beta`. Stable tags are meant to be immutable; beta tags are deleted and recreated.

## Update

`everything update [--beta] [--check] [--allow-unsigned]` updates the running binary from these releases (`src/update.rs`).

- It lists `GET <base>/repos/izyuumi/everything/releases?per_page=100`, where `<base>` is `EVERYTHING_UPDATE_BASE_URL` or `https://api.github.com`, and downloads assets from each asset's `browser_download_url`, so a local fixture serving that JSON and its assets stands in for GitHub. Requests are unauthenticated (GitHub allows 60 per hour per address) and bounded (4 MiB of metadata, 512 MiB per tarball); Ctrl-C cancels them.
- Stable picks the highest non-draft, non-prerelease `v<x.y.z>` release; `--beta` the `v<x.y.z>-beta` release. Either way it reads `candidate.json`, refuses a `version` that disagrees with the tag, and compares `version` then `build` with the running `everything --version` (an ordinary `cargo build` counts as build 0). A beta of the same version with a higher build is newer; nothing older or equal is installed, so the stable channel never downgrades a newer beta.
- It prints one machine-readable line, `channel=<stable|beta> current=<v> available=<v> tag=<tag> status=<newer|up-to-date>`, then a sentence. `--check` stops there with exit 0.
- Otherwise it downloads `SHA256SUMS` and `everything-<target>.tar.gz` for the compile-time target and refuses a digest mismatch before extracting. The tarball is extracted with the `self_update` crate (default features off; `reqwest`, `rustls`, `archive-tar`, `compression-tar-gz`) into a private temporary directory that is removed afterwards.
- On macOS it then applies the signature policy below. It runs the extracted binary's `--version` and refuses unless it prints `everything <version>+<build>` from `candidate.json`.
- It replaces the canonicalized `current_exe()` path in place: a mode-755 temporary file in the same directory is written, synced and renamed over it. A symlinked install updates the link target.
- If the launchd service `com.everything.host` is installed and loaded and its plist's program is this same executable, it runs `launchctl kickstart -k gui/<uid>/com.everything.host` and says so. Otherwise it leaves services alone. Linux has no service step. If the binary was replaced but the restart fails, the command exits 1 with an error that says the new version was installed and to restart the service with `everything service stop` and `everything service start`.

Signature policy (macOS): it reads `codesign -dv --verbose=4` (`Identifier`, `TeamIdentifier`, `Signature=adhoc`), `codesign --verify --strict` and the designated requirement of both the running and the extracted binary. When the running binary is signed by a team, the update must pass strict verification and carry the same team and the identifier `ai.everything.cli`; an unsigned, ad-hoc, other-team or other-identifier binary is refused with an explanation that the Sign in with ChatGPT keychain item would become unreadable. `--allow-unsigned` overrides that refusal with a warning; afterwards sign in again, or re-sign with `scripts/sign_macos.sh`. Same team but a different designated requirement (for example Apple Development to Developer ID) is allowed with a warning that keychain items may need a new sign-in. When the running binary is unsigned or ad-hoc, it prints a note and installs.

`SHA256SUMS` comes from the same release as the tarball, so it guards against corruption, not against a tampered release: whoever can replace the tarball can replace `SHA256SUMS` too. On macOS the authenticity check is the codesign team-ID and identifier policy above. On Linux there is none beyond HTTPS to GitHub. `zipsign` signatures (the crate's `signatures` feature) would close that gap later; they are not used today because nothing in the release flow produces them.

## macOS signing and notarization

`scripts/sign_macos.sh [binary ...]` is the one signing script, for local development ([host guide](host.md#keep-the-chatgpt-sign-in-across-rebuilds-and-updates)) and release CI alike. Without arguments it signs `target/debug/everything` and `target/release/everything` where present.

- Identifier: always `ai.everything.cli`.
- Identity: `SIGN_IDENTITY`, else `EVERYTHING_CODESIGN_IDENTITY`, else the first `Developer ID Application`, else the first `Apple Development` identity from `security find-identity -v -p codesigning`. With none, it exits 1, except under GitHub Actions (`GITHUB_ACTIONS=true`), where it prints a `::warning::` and leaves the binary unsigned so the release still publishes.
- Flags: hardened runtime and a secure timestamp when `NOTARY_PROFILE` is set, `EVERYTHING_CODESIGN_RELEASE=1` or under GitHub Actions; otherwise `--timestamp=none`, so local signing works offline.
- It always verifies with `codesign --verify --strict`, checks the identifier and prints the designated requirement.
- Notarization runs only when `NOTARY_PROFILE` names a `notarytool` keychain profile: it zips the binary in a temporary directory (removed on every exit) and submits it, up to three tries, requiring `Accepted`. A bare Mach-O binary cannot be stapled, so Gatekeeper checks the ticket online.

Keychain items written by a binary signed with one certificate (for example Apple Development) are not readable by a binary signed with another (Developer ID), even with the same identifier, because the item trusts the writer's designated requirement. Use one kind consistently: a host installed from a release (Developer ID) cannot read the ChatGPT sign-in a locally Apple Development–signed build saved, and the reverse; sign in again after switching.

In `release.yml` the macOS build jobs import the identity into a temporary keychain from these repository secrets, by name: `DEVELOPER_ID_P12` (base64 Developer ID Application `.p12`), `DEVELOPER_ID_PASSWORD`, and for notarization `ASC_KEY_ID`, `ASC_ISSUER_ID`, `ASC_KEY_P8` (base64 App Store Connect API key). Absent secrets produce a visible workflow warning and unsigned or unnotarized binaries; the release still publishes.

## Install

`install.sh` at the repository root installs the binary for the current machine:

```sh
curl -fsSL https://raw.githubusercontent.com/izyuumi/everything/main/install.sh | sh
curl -fsSL https://raw.githubusercontent.com/izyuumi/everything/main/install.sh | sh -s -- --beta
```

Options: `--beta` (the current `v<version>-beta`), `--version <tag>`, `--dir <directory>` (default `EVERYTHING_INSTALL_DIR`, else `~/.local/bin`). It downloads the tarball and `SHA256SUMS`, refuses a digest mismatch, installs `everything` and prints its `--version`. `EVERYTHING_DOWNLOAD_URL` replaces the release download base (for example a `file://` directory) to exercise it against a local build. Until the first stable release exists, only `--beta` and `--version` work.

## Local build

```sh
VERSION=0.1.0 BUILD=1 scripts/release_build.sh aarch64-apple-darwin /tmp/everything-dist
```

It writes `/tmp/everything-dist/everything-aarch64-apple-darwin.tar.gz`. A macOS target is signed first through `scripts/sign_macos.sh`, which locally auto-detects an identity and fails without one.

## Owner setup not done by this change

- Releases are off. `release.yml` (every job, including manual runs) and `promote.yml` run only when the repository variable `EVERYTHING_RELEASES` is `enabled`, so merging this publishes nothing. Set it after adding the signing secrets:

  ```sh
  gh variable set EVERYTHING_RELEASES --body enabled -R izyuumi/everything
  ```

- Repository setting "Allow GitHub Actions to create and approve pull requests" is off, so `release_pr.sh` cannot open the release PR until it is enabled; the beta still publishes first.
- No tag ruleset exists. To make stable tags immutable, add a ruleset on `v*` tags that blocks update and deletion, excluding `v*-beta`, which the beta flow deletes.
- None of the signing and notarization secrets above exist yet (repository secret list empty on 2026-10-31), so betas ship unsigned macOS binaries until they are added.

# Releases

How Vorra is versioned, and how the `vorra` CLI and the notarized Mac app are published as a GitHub Releases beta and promoted to stable, and the iOS app uploaded to TestFlight and submitted to the App Store. The scheme mirrors izyuumi/yorozu's `docs/release.md`; Vorra differs in shipping the CLI tarballs and the Mac DMG with Sparkle on the same releases, and in gating every publishing path behind the repository variable `VORRA_RELEASES`. The relay and the optional embedding binaries are not released.

## Version and build numbers

- The version follows SemVer and is the same for the CLI and the Apple apps. `scripts/version.sh` computes it from the Conventional Commits since the last stable tag `v<major.minor.patch>` (prerelease tags such as `v0.2.0-beta` are ignored): a breaking change (`type!:` or a `BREAKING CHANGE:` footer) bumps major (minor while below 1.0), a `feat` bumps minor, anything else patch. Nobody picks it by hand. The scripts need the tags (`fetch-depth: 0` in CI). `VERSION=<x.y.z>` overrides it for local builds; `--publish` refuses a beta tag that differs from `scripts/version.sh`.
- The 0.x rule is Yorozu's, kept so both projects read the same: below 1.0 a breaking change and a `feat` both bump minor, so `0.y.z` patch releases carry only fixes and chores. Cargo's 0.x compatibility rule (minor is the compatibility number) holds as a consequence. From 1.0 it is plain SemVer.
- Start: with no stable tag, the history's `feat` commits give `0.1.0`, the first release.
- Placeholders: `Cargo.toml`'s package version is the fixed `0.1.0` and never bumped (the release PR does not touch it, so merging the PR starts no new beta). The Apple project holds `MARKETING_VERSION = 0.1.0`, `CURRENT_PROJECT_VERSION = 100001` and `VORRA_BUILD = 1` once, at project level, so local Xcode builds stay sane; both `Info.plist`s read `$(MARKETING_VERSION)`, `$(CURRENT_PROJECT_VERSION)` and `VorraBuild = $(VORRA_BUILD)`. Every script passes the real `MARKETING_VERSION`, `CURRENT_PROJECT_VERSION` and `VORRA_BUILD` to `xcodebuild`.
- The CLI build restarts at 1 per version: the current `v<version>-beta` release's `candidate.json` build plus one, or 1 (`scripts/release.sh --next`). The Mac app in the same release uses that same build: its `VorraBuild` is `<build>` and its `CFBundleVersion` is `build-number.sh <version> <build>`, so `vorra <version>+<build>` and the Mac app of one release always match. The iOS build is the highest build App Store Connect has for this version plus one (`scripts/upload_ios.sh`); 0.1.0 build 1 (100001) is already on TestFlight, so the next 0.1.0 upload is build 2 (100002).
- `CFBundleVersion` encodes version and build as one integer, `scripts/build-number.sh <version> <build>`:

  ```
  major·10⁷ + minor·10⁵ + patch·10³ + build      e.g. 0.1.0 build 2 = 100002
  ```

  It exits 64 on a non-numeric input and 65 unless minor and patch are below 100 and build is 1 to 999. Versions only rise and the build only rises within a version, so the integer only rises and never repeats.

### One stamped version

The release build sets `VORRA_VERSION=<version>+<build>` at compile time (`scripts/release_build.sh`, which also checks that a host-target binary's `--version` prints exactly `vorra <version>+<build>`). `vorra::VERSION` is that value, or the `Cargo.toml` placeholder for an ordinary `cargo build`. Everything that reports a version reads it: `vorra --version`, `vorra update`'s current version and user agent, the Anthropic client's user agent, the MCP client `Implementation`, the local transport's host status, the `vorra tui` title and the embedding benchmark report. `vorra update` compares it with `candidate.json`'s `version` and `build`, which `--publish` writes from the same `VERSION` and `BUILD`. A help or MCP tool reporting the running version (such as the planned help MCP's `latest_version`, #261) should report `vorra::VERSION`; one reporting the latest release should read the newest non-prerelease `v<x.y.z>` the way `vorra update` does.

### Drift guard

`scripts/check_version.sh [<tag>]` runs in the `release-guards` job of `validate.yml` on every push and pull request. It fails when `Cargo.toml`'s package version is not the `0.1.0` placeholder, when `Cargo.lock`'s `vorra` entry differs from `Cargo.toml`, when the Apple project sets any `MARKETING_VERSION`, `CURRENT_PROJECT_VERSION` or `VORRA_BUILD` other than the placeholders, when an `Info.plist` hard-codes `CFBundleShortVersionString`, `CFBundleVersion` or `VorraBuild`, or when Rust code outside `src/lib.rs` reads `CARGO_PKG_VERSION` or `VORRA_VERSION` instead of `vorra::VERSION`. So no file holds a real version, and the stamped `VORRA_VERSION` is the only version a binary reports; at release `candidate.json`, the tag and `vorra --version` carry the same `VERSION` and `BUILD`. `Cargo.toml` stays the placeholder because a release PR that bumped it would start a new beta when merged; Yorozu's release PR touches only `CHANGELOG.md` for the same reason. With a tag (as `--publish` passes `v<version>-beta`) it also fails unless the tag's version is what `scripts/version.sh` computes at that commit. `--promote` checks that `candidate.json`'s version is the beta tag's.

## CI

| Workflow | Trigger | Does |
| --- | --- | --- |
| `validate.yml` | every push to `main` and pull request | `release-guards`: `scripts/check_version.sh`, `scripts/external_testflight.sh --audit` and `scripts/release_gate.sh`. `rust`: fmt and strict Clippy. |
| `apple-validation.yml` | push and pull request touching Apple or shared-protocol paths | Build and analyze `VorraMac` and `VorraIOS` unsigned with `MARKETING_VERSION=$(scripts/version.sh)`, `CURRENT_PROJECT_VERSION` from `build-number.sh <version> 1` and `VORRA_BUILD=1`. |
| `release.yml` | push to `main` except a change to only `CHANGELOG.md`; manual | `next`: `scripts/release.sh --next`. `build`: `scripts/release_build.sh <target>` on each target, signing macOS binaries when the secrets exist. `mac`: `scripts/release_mac.sh` (Mac DMG and appcast) when its secrets exist. `publish`: `scripts/release.sh --publish`, which replaces the beta and opens or updates the release PR. |
| `promote.yml` | the release PR (branch `release`, same repository) merged | `scripts/release.sh --promote <build> <sha> <digest>` with the PR body's `Release-Build:`, `Release-Commit:` and `Release-Digest:` lines. |
| `release-ios.yml` | push to `main` touching `apps/Apple/**`, `scripts/upload_ios.sh`, `scripts/asc_lib.sh`, `scripts/version.sh` or `scripts/build-number.sh`; manual | `scripts/upload_ios.sh --confirm`. Concurrency group `release-ios`, one upload at a time. |

`release.yml` and `promote.yml` share the `release` concurrency group without cancellation, so a promote and the next beta never interleave. `release.yml`, `promote.yml` and `release-ios.yml` run only in `izyuumi/everything` and only while the repository variable `VORRA_RELEASES` is `enabled`; otherwise every job is skipped. `scripts/release.sh --publish|--promote`, `scripts/release_pr.sh`, `scripts/upload_ios.sh --confirm` and the write flags of `scripts/app_store_submit.sh` also refuse unless the environment has `VORRA_RELEASES=enabled`, which the workflows pass from the variable. `scripts/release_gate.sh`, in `release-guards`, fails a pull request when a workflow job that can publish, tag, upload, notarize, move refs or open the release PR (`gh release create|delete|edit|upload`, `git tag`, `git push`, `gh pr create|edit|merge`, `git/refs`, `release.sh --publish|--promote`, `release_pr.sh`, `upload_ios.sh --confirm`, `app_store_submit.sh`, `notarytool`, `altool`, `contents: write`, `pull-requests: write`) has no job `if:` containing `vars.VORRA_RELEASES == 'enabled'`, or when a script that makes such writes lacks the `VORRA_RELEASES` check. The value stays `enabled`, which #226 chose and its owner setup command uses, rather than the `true` that Yorozu's porting note suggests. Both are exact-string checks that refuse every other value, so `true` would add no safety and would only break the documented command. The release workflows run no tests; the normal [POC CI](poc-ci.md) gates merges. They run on `macos-latest` (Apple) and `ubuntu-*` (Linux CLI) with the runner's default Xcode.

Secrets, by name: `release.yml` uses `DEVELOPER_ID_P12`, `DEVELOPER_ID_PASSWORD`, `ASC_KEY_ID`, `ASC_ISSUER_ID`, `ASC_KEY_P8` and, in its `mac` job, `SPARKLE_ED_KEY` plus the repository variable `VORRA_SPARKLE_PUBLIC_KEY`; `release-ios.yml` uses `MAC_CERT_P12`, `MAC_CERT_PASSWORD` (the Apple Distribution identity, despite the name), `ASC_KEY_ID`, `ASC_ISSUER_ID`, `ASC_KEY_P8`.

Write any `run:` that contains `: ` as a `|` block scalar (as `promote.yml` does for its `Release-Build:` parsing): in a plain scalar it starts a YAML mapping, and the workflow then fails to parse with no jobs, as Yorozu's `promote.yml` once did. Run `actionlint` on all workflows after editing one.

Targets and runners: `aarch64-apple-darwin` and `x86_64-apple-darwin` on `macos-latest` (x86_64 cross-compiled), `x86_64-unknown-linux-gnu` on `ubuntu-22.04` and `aarch64-unknown-linux-gnu` on `ubuntu-22.04-arm`. The Linux binaries need glibc 2.35 or newer.

## Beta

Every push to `main` republishes the single prerelease `v<version>-beta`: `--publish` refuses a dirty tree, deletes the current beta release and its tag, then creates `v<version>-beta` at the pushed commit, not marked latest, with the assets below and notes from `scripts/release_notes.sh` (feat, fix and perf Conventional Commits since the last stable tag, dependency bumps left out, PR numbers kept). There is one beta at a time; when a `feat` lands the next beta moves to the new minor version.

## Stable

Stable releases work like release-please. After each beta, `scripts/release_pr.sh` resets the branch `release` to the beta's commit, commits `CHANGELOG.md` with the version's notes through the contents API (GitHub signs that commit), opens or updates the PR `chore: release <version>`, whose body ends with `Release-Build: <build>`, `Release-Commit: <source sha>` and `Release-Digest: sha256:<SHA256SUMS digest>`, and approves that PR's `validate.yml` and `apple-validation.yml` runs, which wait at `action_required` because a bot opened the PR (a dispatched run does not count toward the PR's checks, as Yorozu found). If no run is waiting or the token may not approve, it warns; approve or rerun the checks on the PR by hand.

Check that beta, then merge the PR. `--promote` then republishes the beta's assets unchanged as `v<version>`, marked latest, and deletes the beta release and tag. Before publishing anything it checks that `candidate.json`'s build, tag, version and source commit, the release's target commit, the commit the beta tag names, the `SHA256SUMS` digest on GitHub and as downloaded, and every tarball against `SHA256SUMS` all match what the PR named; every mismatch is reported and it exits 1 without publishing or deleting anything. A newer beta updates the PR to name it. It does nothing if `v<version>` already exists. A missing or malformed argument exits 64.

Merging the release PR changes only `CHANGELOG.md`, so it starts no new beta; the next push does.

Release notes come from `scripts/release_notes.sh [<commit>]`: the pull requests merged into `main` since the last stable tag (never a `-beta` tag), grouped by the feat, fix and perf types of their titles, dependency bumps and other types left out. `--what-to-test <locale> <version> <build> [<commit>]` gives the same list as plain text within App Store Connect's 4,000 bytes for a TestFlight build, with the intro, headings and closing line from `apps/Apple/metadata/testflight/<locale>/what_to_test.txt` (`en-US` only).

## iOS and iPadOS: TestFlight

`scripts/upload_ios.sh --confirm` archives `VorraIOS` for `generic/platform=iOS` (Release, automatic signing, `-allowProvisioningUpdates` with the App Store Connect key) and uploads it: `apps/Apple/iOS/ExportOptions.plist` has `destination` `upload`, so the export step is the upload. Each upload uses up its build number.

- Without `--confirm` it exits 64; without `VORRA_RELEASES=enabled` it exits 1. Needs `ASC_KEY_ID`, `ASC_ISSUER_ID` and the `.p8` key (`ASC_KEY_PATH`, else `~/.appstoreconnect/private_keys/AuthKey_<id>.p8`); `scripts/asc_lib.sh` signs the API token (app ID `6829295744`, "Vorra AI") and keeps the key and token out of arguments and output.
- Visible build (`VORRA_BUILD`, Info.plist `VorraBuild`): the highest build App Store Connect has for this version, plus one; `BUILD=<n>` skips the lookup. After uploading it waits up to `LIST_WAIT` seconds (default 3600) until App Store Connect lists the build and fails if it never does, so with the `release-ios` concurrency group the next upload cannot reuse its number. `build-number.sh` rejects a build past 999: promote, so the next build starts a new version.
- Any other TestFlight upload, by hand or from another workstream (for example the pairing-UX work), must go through `scripts/upload_ios.sh` or pass `CURRENT_PROJECT_VERSION=$(scripts/build-number.sh <version> <App Store Connect's highest build for that version + 1>)` with the matching `MARKETING_VERSION` and `VORRA_BUILD`. For 0.1.0 that is 100002 next: 100001 is already on TestFlight and a number is never reused. Use a normalized count like `upload_ios.sh` does (an App Store Connect value above `build-number.sh <version> 1` minus 1 counts as that value minus it). Do not set `testFlightInternalTestingOnly`: it would also block App Store submission of those builds.
- After the upload it sets the build's What to Test from `release_notes.sh --what-to-test` in each locale folder under `apps/Apple/metadata/testflight`. A locale the build lacks is created; one it has is set only while empty or still holding this script's text, so text written by hand stays. TestFlight gives processed builds to the internal group only.
- External TestFlight is off. Nothing gives a build to an external group or submits it to Beta App Review. A step that ever does must run `scripts/external_testflight.sh` first and stop unless it exits 0, which it does only when the repository variable `VORRA_EXTERNAL_TESTFLIGHT` (passed by `release-ios.yml`, unset) is exactly `true`. `scripts/external_testflight.sh --audit`, in `validate.yml`'s `release-guards` job, fails a pull request whose workflows or scripts name an App Store Connect group or Beta App Review call (`betaGroups`, `betaAppReviewSubmissions`, …) without running the switch.

## App Store submission

`scripts/app_store_submit.sh <version> <build> [--release-type MANUAL|AFTER_APPROVAL] [--create] [--attach] [--submit]`, run by hand and never in CI. `<build>` is the `CFBundleVersion`. Without a flag it only reads App Store Connect and prints its plan, exiting 65 with the reason if anything would stop it. `--create` creates the iOS version if missing, else reuses it, and sets its release type (default: that of the newest version on sale, so the first submission needs `--release-type`). `--attach` attaches the build only if it is `VALID`, not expired and uploaded for that version. `--submit` reuses an open review submission or creates one, adds the version once and submits it. It reports the content rights declaration and the build's export compliance against `ITSAppUsesNonExemptEncryption` in `apps/Apple/iOS/Info.plist`, and writes nothing while any of these block. A write flag also needs `VORRA_RELEASES=enabled`. Exit codes: 64 bad arguments, 65 blocked, 70 App Store Connect refused a write, 1 no key. For example `scripts/app_store_submit.sh 0.1.0 100002 --release-type MANUAL`, then with `--create --attach`, then `--submit` once the owner approves.

Not ported from Yorozu yet: `testflight_metadata.sh` and `app_store_metadata.sh` (beta description and App Store listing from files), `app_store_media.sh` (screenshots); set those in App Store Connect by hand for now.

## Mac: beta and stable releases

The Mac app ships on the same releases as the CLI: one channel set, no separate Mac tags. `release.yml`'s `mac` job runs `scripts/release_mac.sh <dir>` on `macos-latest` with the `next` job's `VERSION` and `BUILD`, and `publish` adds its output to the beta:

1. Release build of `VorraMac` with `MARKETING_VERSION`, `CURRENT_PROJECT_VERSION` (`build-number.sh`), `VORRA_BUILD` and `VORRA_SPARKLE_PUBLIC_KEY` on the command line, manual Developer ID signing (`SIGN_IDENTITY`), hardened runtime, secure timestamp, no entitlements; re-signs Sparkle's `Installer.xpc`, `Downloader.xpc`, `Autoupdate`, `Updater.app`, the framework, then the app (inside out); checks the bundle id is `to.yumi.vorra.mac` (the id Sparkle updates), the `CFBundleVersion`, the public key and that `SUFeedURL` is the stable appcast.
2. `Vorra.dmg` (the app and an `/Applications` link): signed, notarized with the notarytool keychain profile `NOTARY_PROFILE` (up to three tries), stapled and checked with `spctl`.
3. `appcast.xml` from Sparkle's `generate_appcast --ed-key-file "$SPARKLE_ED_KEY_FILE" --download-url-prefix https://github.com/<repo>/releases/download/v<version>-beta/`; it must carry the `CFBundleVersion` and the beta DMG URL.

It refuses without `VORRA_RELEASES=enabled`, `SIGN_IDENTITY`, `NOTARY_PROFILE`, `SPARKLE_ED_KEY_FILE` or `VORRA_SPARKLE_PUBLIC_KEY`, so it runs only in CI; a local build without signing is the plain `xcodebuild` in the [Mac client guide](macos-client.md). When `DEVELOPER_ID_P12`, `DEVELOPER_ID_PASSWORD`, `ASC_KEY_ID`, `ASC_ISSUER_ID`, `ASC_KEY_P8`, `SPARKLE_ED_KEY` or the variable `VORRA_SPARKLE_PUBLIC_KEY` is missing, the job warns and the beta publishes without the Mac assets.

`scripts/release.sh --publish` takes `Vorra.dmg` and `appcast.xml` only together, checks the appcast's `sparkle:version` and beta URL, adds `Vorra.dmg` to `SHA256SUMS` and writes the `mac` entry into `candidate.json`. So the release PR's one `Release-Digest` (of `SHA256SUMS`) pins the tarballs and the DMG. `--promote` additionally checks that `candidate.json`'s Mac `bundle_version` is `build-number.sh <version> <build>`, that `SHA256SUMS` lists `Vorra.dmg` (whose bytes the `shasum -c` covers), and that the appcast carries that version and the beta URL; it then rewrites the appcast's `/releases/download/v<version>-beta/` URLs to `/releases/download/v<version>/` and republishes the same signed, notarized DMG, so nothing is rebuilt. The EdDSA signature covers the DMG, not the feed, so the rewrite keeps it valid.

Feeds:

- Stable: `SUFeedURL` is `https://github.com/izyuumi/everything/releases/latest/download/appcast.xml` (build setting `VORRA_SPARKLE_FEED_URL`), the latest stable release's appcast. Until a stable release carries the Mac assets this URL is a 404; scheduled checks then fail quietly and Check Now shows the error.
- Beta: GitHub has no fixed URL for the newest prerelease, and there is no Vorra website to redirect from as Yorozu's `/beta/appcast.xml` does. With Update to: Beta Versions, the app reads `GET https://api.github.com/repos/izyuumi/everything/releases?per_page=100` (unauthenticated, at most 4 MiB, at launch, hourly, before Check Now and when the switch changes), takes the first non-draft prerelease tagged `v<x.y.z>-beta`, as `vorra update --beta` does, and caches its `appcast.xml` asset URL in `UserDefaults` (`VorraBetaFeedURL`) only when it is `https://github.com/izyuumi/everything/releases/download/<that tag>/appcast.xml`. `feedURLString(for:)` returns it; with no beta (after promotion deletes it) the cache is cleared and Sparkle uses the stable feed, as Yorozu's beta feed falls back to the newest release. A failed lookup keeps the last URL. This needs no new infrastructure and uses the same release selection as the CLI; the cost is one API call an hour per beta user, within GitHub's 60 per hour.

The app (`apps/Apple/macOS/MacUpdates.swift`) creates `SPUStandardUpdaterController` at launch only when `SUFeedURL` and `SUPublicEDKey` are non-empty; `VORRA_SPARKLE_PUBLIC_KEY` is empty in the project, so local and `apple-validation.yml` builds have no updater, Settings › General › Updates says so and the menu bar panel's Check for Updates… is disabled. On first run it turns on automatic checks and automatic download and install with a one-hour interval (`VorraUpdatesConfigured`); `Info.plist` also sets `SUEnableAutomaticChecks`, `SUAutomaticallyUpdate`, `SUAllowsAutomaticUpdates` and `SUScheduledCheckInterval` 3600. Sparkle's standard user driver shows the update alerts. Settings › General › Updates has Automatically Check for Updates, Install Updates Automatically, Update to: Stable Versions / Beta Versions (`VorraBetaUpdates`), Check Now, the last check and its outcome.

## Asset contract

Each release, beta or stable, carries exactly these assets. Names never contain the version, so a stable install can use `releases/latest/download/<asset>`.

| Asset | Content |
| --- | --- |
| `vorra-<target>.tar.gz` | gzip tar holding one file, `vorra`, at the archive root, mode 755. `<target>` is one of `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`. |
| `Vorra.dmg` | The notarized, stapled Mac app (`to.yumi.vorra.mac`), when the `mac` job had its secrets. |
| `appcast.xml` | Sparkle feed for `Vorra.dmg`, with the EdDSA enclosure signature and `sparkle:version` = the `CFBundleVersion`; present exactly when `Vorra.dmg` is. |
| `SHA256SUMS` | `shasum -a 256` lines (`<hex>  <asset>`) for every tarball and `Vorra.dmg`. `appcast.xml` is left out because promotion rewrites its URLs. |
| `candidate.json` | `{"schema":1,"tag","version","build" (integer),"source_branch":"main","source_sha","assets":[{"target","name"}]}`, plus `"mac":{"asset":"Vorra.dmg","appcast":"appcast.xml","bundle_version"}` when the Mac app is included. |

Tags: stable `v<major.minor.patch>`; beta `v<major.minor.patch>-beta`. Stable tags are meant to be immutable; beta tags are deleted and recreated.

## Update

`vorra update [--beta] [--check] [--allow-unsigned]` updates the running binary from these releases (`src/update.rs`).

- It lists `GET <base>/repos/izyuumi/everything/releases?per_page=100`, where `<base>` is `VORRA_UPDATE_BASE_URL` or `https://api.github.com`, and downloads assets from each asset's `browser_download_url`, so a local fixture serving that JSON and its assets stands in for GitHub. Requests are unauthenticated (GitHub allows 60 per hour per address) and bounded (4 MiB of metadata, 512 MiB per tarball); Ctrl-C cancels them.
- Stable picks the highest non-draft, non-prerelease `v<x.y.z>` release; `--beta` the `v<x.y.z>-beta` release. Either way it reads `candidate.json`, refuses a `version` that disagrees with the tag, and compares `version` then `build` with the running `vorra --version` (an ordinary `cargo build` counts as build 0). A beta of the same version with a higher build is newer; nothing older or equal is installed, so the stable channel never downgrades a newer beta.
- It prints one machine-readable line, `channel=<stable|beta> current=<v> available=<v> tag=<tag> status=<newer|up-to-date>`, then a sentence. `--check` stops there with exit 0.
- Otherwise it downloads `SHA256SUMS` and `vorra-<target>.tar.gz` for the compile-time target and refuses a digest mismatch before extracting. The tarball is extracted with the `self_update` crate (default features off; `reqwest`, `rustls`, `archive-tar`, `compression-tar-gz`) into a private temporary directory that is removed afterwards.
- On macOS it then applies the signature policy below. It runs the extracted binary's `--version` and refuses unless it prints `vorra <version>+<build>` from `candidate.json`.
- It replaces the canonicalized `current_exe()` path in place: a mode-755 temporary file in the same directory is written, synced and renamed over it. A symlinked install updates the link target.
- On macOS it first unloads and removes a leftover `com.everything.host` LaunchAgent, if present. If the launchd service `to.yumi.vorra.host` is installed and loaded and its plist's program is this same executable, it runs `launchctl kickstart -k gui/<uid>/to.yumi.vorra.host` and says so. Otherwise it leaves services alone. Linux has no service step. If the binary was replaced but the restart fails, the command exits 1 with an error that says the new version was installed and to restart the service with `vorra service stop` and `vorra service start`.

Signature policy (macOS): it reads `codesign -dv --verbose=4` (`Identifier`, `TeamIdentifier`, `Signature=adhoc`), `codesign --verify --strict` and the designated requirement of both the running and the extracted binary. When the running binary is signed by a team, the update must pass strict verification and carry the same team and the identifier `to.yumi.vorra.cli`; an unsigned, ad-hoc, other-team or other-identifier binary is refused with an explanation that the Sign in with ChatGPT keychain item would become unreadable. `--allow-unsigned` overrides that refusal with a warning; afterwards sign in again, or re-sign with `scripts/sign_macos.sh`. Same team but a different designated requirement (for example Apple Development to Developer ID) is allowed with a warning that keychain items may need a new sign-in. When the running binary is unsigned or ad-hoc, it prints a note and installs.

`SHA256SUMS` comes from the same release as the tarball, so it guards against corruption, not against a tampered release: whoever can replace the tarball can replace `SHA256SUMS` too. On macOS the authenticity check is the codesign team-ID and identifier policy above. On Linux there is none beyond HTTPS to GitHub. `zipsign` signatures (the crate's `signatures` feature) would close that gap later; they are not used today because nothing in the release flow produces them.

## macOS signing and notarization

`scripts/sign_macos.sh [binary ...]` is the one signing script, for local development ([host guide](host.md#keep-the-chatgpt-sign-in-across-rebuilds-and-updates)) and release CI alike. Without arguments it signs `target/debug/vorra` and `target/release/vorra` where present.

- Identifier: always `to.yumi.vorra.cli`.
- Identity: `SIGN_IDENTITY`, else `VORRA_CODESIGN_IDENTITY`, else the first `Developer ID Application`, else the first `Apple Development` identity from `security find-identity -v -p codesigning`. With none, it exits 1, except under GitHub Actions (`GITHUB_ACTIONS=true`), where it prints a `::warning::` and leaves the binary unsigned so the release still publishes.
- Flags: hardened runtime and a secure timestamp when `NOTARY_PROFILE` is set, `VORRA_CODESIGN_RELEASE=1` or under GitHub Actions; otherwise `--timestamp=none`, so local signing works offline.
- It always verifies with `codesign --verify --strict`, checks the identifier and prints the designated requirement.
- Notarization runs only when `NOTARY_PROFILE` names a `notarytool` keychain profile: it zips the binary in a temporary directory (removed on every exit) and submits it, up to three tries, requiring `Accepted`. A bare Mach-O binary cannot be stapled, so Gatekeeper checks the ticket online.

Keychain items written by a binary signed with one certificate (for example Apple Development) are not readable by a binary signed with another (Developer ID), even with the same identifier, because the item trusts the writer's designated requirement. Use one kind consistently: a host installed from a release (Developer ID) cannot read the ChatGPT sign-in a locally Apple Development–signed build saved, and the reverse; sign in again after switching.

Identifier history: binaries up to and including commit `d102e43` were signed as `ai.everything.cli`, later Everything binaries as `to.yumi.everything.cli`, and Vorra binaries as `to.yumi.vorra.cli`.

### First Vorra release: install manually once

The product was renamed from Everything to Vorra. The first Vorra release changes the binary name (`vorra`), the release asset name (`vorra-<target>.tar.gz`), the signing identifier (`to.yumi.vorra.cli`) and every keychain service name (`to.yumi.vorra.*`). An old `everything update` looks for `everything-<target>.tar.gz` and refuses the new identifier, so it cannot install Vorra. Release notes for that release must say:

1. Install Vorra once manually with `install.sh` (it installs `vorra` next to the old `everything` binary, which can then be deleted).
2. Run any `vorra` command once. When `~/.vorra` is absent and `~/.everything` exists, it renames `~/.everything` to `~/.vorra` (falling back to a symlink `~/.vorra -> ~/.everything` if the rename fails) and renames `everything.sqlite3` and its `-wal`/`-shm`/`-journal` files to `vorra.sqlite3`; nothing is copied or deleted. It refuses while an old host still holds the state lock. After a real rename, whenever `~/.vorra` is a directory or symlink and `~/.everything` is gone, it then rewrites absolute paths stored under `~/.everything` (as given or through a symlinked `$HOME`) to the same place under `~/.vorra`: every path-valued string in `host-settings-v1.json` (owner, worker and selection workspace roots, MCP executables and working directories, memory runtime and model paths, vault binary and session paths), with the settings revision bumped, and the definitions of scheduled jobs in `vorra.sqlite3`, with generation and revision bumped. Paths outside the old directory are left alone, the step is a no-op once nothing points there, and it is skipped while a host holds the lock and when the symlink fallback keeps `~/.everything`. Recorded history (task inputs, command options, conversation run records and already claimed schedule runs) keeps the old paths. The device identity, pairing keys, relay database, memory databases and logs store no absolute paths, and `vorra service install` rewrites the LaunchAgent plist.
3. Run `vorra service install` (and `vorra service start`) if a LaunchAgent was used; it unloads and removes the old `com.everything.host` LaunchAgent and installs `to.yumi.vorra.host`.
4. Run `vorra chatgpt sign-in` again and re-enter any vault, MCP or System One credentials. Keychain items under the old `ai.everything.*` services are not read.

In `release.yml` the macOS build jobs import the identity into a temporary keychain from these repository secrets, by name: `DEVELOPER_ID_P12` (base64 Developer ID Application `.p12`), `DEVELOPER_ID_PASSWORD`, and for notarization `ASC_KEY_ID`, `ASC_ISSUER_ID`, `ASC_KEY_P8` (base64 App Store Connect API key). Absent secrets produce a visible workflow warning and unsigned or unnotarized binaries; the release still publishes.

## Install

`install.sh` at the repository root installs the binary for the current machine:

```sh
curl -fsSL https://raw.githubusercontent.com/izyuumi/everything/main/install.sh | sh
curl -fsSL https://raw.githubusercontent.com/izyuumi/everything/main/install.sh | sh -s -- --beta
```

Options: `--beta` (the current `v<version>-beta`), `--version <tag>`, `--dir <directory>` (default `VORRA_INSTALL_DIR`, else `~/.local/bin`). It downloads the tarball and `SHA256SUMS`, refuses a digest mismatch, installs `vorra` and prints its `--version`. `VORRA_DOWNLOAD_URL` replaces the release download base (for example a `file://` directory) to exercise it against a local build. Until the first stable release exists, only `--beta` and `--version` work.

## Local build

```sh
VERSION=0.1.0 BUILD=1 scripts/release_build.sh aarch64-apple-darwin /tmp/vorra-dist
```

It writes `/tmp/vorra-dist/vorra-aarch64-apple-darwin.tar.gz`. A macOS target is signed first through `scripts/sign_macos.sh`, which locally auto-detects an identity and fails without one.

## Owner setup not done by this change

- Releases are off. `release.yml` (every job, including manual runs), `promote.yml` and `release-ios.yml` run only when the repository variable `VORRA_RELEASES` is `enabled`, so merging this publishes nothing. Set it after adding the signing secrets:

  ```sh
  gh variable set VORRA_RELEASES --body enabled -R izyuumi/everything
  ```

- Repository setting "Allow GitHub Actions to create and approve pull requests" is off, so `release_pr.sh` cannot open the release PR until it is enabled; the beta still publishes first.
- No tag ruleset exists. To make stable tags immutable, add a `release tags` ruleset on `refs/tags/v*` that blocks update and deletion, excluding `refs/tags/*-beta`, which the beta flow deletes. Without that exclusion `gh release delete --cleanup-tag` deletes the release, the tag deletion is refused and the next beta fails to publish (Yorozu hit this).
- None of the signing and notarization secrets above exist yet (repository secret list empty on 2026-10-31), so betas ship unsigned macOS binaries until they are added. `release-ios.yml` needs `MAC_CERT_P12`, `MAC_CERT_PASSWORD`, `ASC_KEY_ID`, `ASC_ISSUER_ID` and `ASC_KEY_P8` before it can upload.
- Leave `VORRA_EXTERNAL_TESTFLIGHT` unset: builds reach only the internal TestFlight group.
- The Mac app needs a Sparkle EdDSA key pair. On the owner Mac, with Sparkle's tools (after any Xcode build of `VorraMac`, under `<DerivedData or -clonedSourcePackagesDirPath>/artifacts/sparkle/Sparkle/bin`):

  ```sh
  ./generate_keys --account vorra
  ./generate_keys --account vorra -x /tmp/vorra-sparkle-private.key
  gh secret set SPARKLE_ED_KEY -R izyuumi/everything < /tmp/vorra-sparkle-private.key
  rm -P /tmp/vorra-sparkle-private.key
  gh variable set VORRA_SPARKLE_PUBLIC_KEY -R izyuumi/everything --body "$(./generate_keys --account vorra -p)"
  ```

  The first command creates the key in the login keychain (or prints the existing public key), `-x` exports the private key, `-p` prints the public key. Keep the keychain item (or an offline copy of the exported key): losing it means shipped apps can no longer verify updates. Never commit the private key. The public key is not secret; it is a repository variable rather than a committed value so the project builds without one. `DEVELOPER_ID_P12`, `DEVELOPER_ID_PASSWORD`, `ASC_KEY_ID`, `ASC_ISSUER_ID` and `ASC_KEY_P8` are the same secrets the CLI signing uses.

# Native pairing validation

The [QR pairing walkthrough](#qr-pairing-walkthrough-2026-11-01) at the end covers `vorra pair`. This first dated record covers the actual disposable Linux Host/Replica walkthrough and two separate immutable metadata readers completed on 2026-10-16. It verifies the exercised native backend paths from merged PR #138. Native Apple ceremony controls and full [#112](https://github.com/izyuumi/everything/issues/112) acceptance remain open.

## Source and artifacts

The optimized default artifacts were built from `d33ce9ea59960c6226183887e88341e049a3e751`. All 146 selected Rust, build and schema inputs match main `a3c45ec68f64029bbcac76a55081cf3a7bd6b86f`; the later merged changes affect Swift and documentation. This establishes component source identity, without claiming execution of the newer whole tree or an all-feature runtime.

| Retained artifact | SHA-256 |
| --- | --- |
| Default Host/CLI | `900dc5ff3b6d211ccefb2c69ad68ca0e335b7bbd23f2d98ebe97533b40702bf2` |
| Default library | `95eea283ac2e6c13773e47b885714274beecdf839c52f0e3047cada8cab4f6ca` |
| Production Replica helper | `5f8e86a33ee6cbb85583d8fbdde67bab24b6541b76ad077f254a9ac6d01ba51c` |

The Replica helper uses unchanged production device/pairing implementations. Its direct linkage and scoped process lifecycle do not establish relay execution, Apple client behavior or an independent agent runtime. No conversational or classifier request was made, and neither fresh Host had a configured model or relay. Embeddings remained disabled.

## Actual operations

The corrected producer completed once with 84 actual CLI calls and 87 joined process supervisors: 66 expected successes and 21 expected negative exits. Two genuine native sessions used the existing sensitive pipe. A genuine invitation and signed Replica request required a separate explicit final native approval. Host completion supplied the original Completed receipt and committed pin; the production Replica then actually completed pairing. The selected conversation set was empty, with submit/resolve enabled, so the observation grants no conversation content scope.

Exact approval replay retained the original receipt and private acceptance. The nonsecret owner inspection matched the original result. Actual rejected calls covered another genuine native session, blank label, mismatched fingerprint/inviter/access, unknown access fields, more than 64 conversation IDs, nil approval identity and changed replay fields. A separate genuine request with one signature byte changed was rejected by the production verifier. Another ceremony was cancelled before approval.

Cancelling revocation preserved the active pin. A fresh exact revocation completed; the original approval receipt remained immutable and acceptance ceased being returned. No replay or post-revocation Replica completion was performed. Eight retained invitations reached the real capacity limit. The ninth original claim was InFlight before restart and Finished/Unknown afterward; no terminal NoEffects result was invented. Explicit cancellation and native-session close suppressed unused invitation redisplay and rejected fresh approval. Graceful Host restart preserved the original receipt and rejected the old proof.

The helper returned its actual stop/join response. Both Host SIGINT shutdowns completed with exit 0. The observer reaped the actual producer with wait4, retained 26 proven pidfds through their exits and observed all 441 recorded tracer/traced process and thread IDs absent afterward. There was no forced cleanup or observer-issued termination signal. These are actual observed process boundaries, without a general cancellation or crash-recovery claim.

## Independent retained metadata

The quiescent runtime inventory contained 384 entries and four ordinary, private, single-link databases without sockets, WAL, SHM or rollback journals. The separately authorized audit and a distinct independent reader each used explicit SQLite file URIs with `mode=ro&immutable=1`, `uri=True`, verified `query_only=1`, memory-only temporary storage, disabled extensions and bounded public projections. Host and Replica were schema 13, lexical 2 and maintenance 1; all four integrity checks returned only `ok` and foreign-key checks were empty.

Both readers reproduced 17 original NativeUi operations, 16 native claims and 50 updates. Latest outcomes were 14 Completed, one Failed, one Cancelled and one Unknown. Original target, request, binding, native session and native-attempt metadata agreed, including the ninth claim's original InFlight-to-Unknown history. Neither a settings file write nor an application record existed.

Each endpoint retained one exact opposite-device public pin and one genuine pairing-use marker. Public endpoint keys, roles, authoritative Host identity, fingerprint, original approval identifiers and selected access matched. The Host's exact Replica pin was revoked; the Replica's Host pin remained unrevoked. Rejected and cancelled ceremonies added no pairing-use marker. The readers selected known public leaves rather than raw configuration, private identities, proofs, invitation/request/acceptance bytes, credentials or unknown row bodies.

General conversation, event, task, tool, command, replica-event and worker-admission counts were zero in Host/Replica; Wiki pages and maintenance jobs were also zero. These are retained database facts with their stated scope. Fourteen first-audit inventories and ten independent-reader inventories matched the entire original footprint, including hashes, identities, ownership, modes, links, allocation and mtime. The reads created no sidecars. All 430 original protected inputs, 18 retained artifact copies, compiled source inputs, helper sources and failed-run evidence remained unchanged.

## Preserved failures and limitations

The first producer failed because its observer treated a normal Host socket as inadmissible. It reached no native pairing operation. That run remains retained separately. The first audit invocation omitted `uri=True` and raised OperationalError before a connection or query. Its invocation text was saved afterward; it is not counted as an immutable read or a source file hashed before execution. The explicit URI correction proceeded only after the complete footprint was rechecked. Neither failure was discarded or treated as success.

The successful producer has a known marker-coverage defect. Its 103 RAM-only known values were scanned across 28 files in the new state root plus the old evidence root; the new evidence and trace root was excluded by two unchanged hardcoded paths. Zero reported matches therefore do not establish complete output coverage. The values and their hashes were not persisted, so the audit cannot reproduce or repair that scan. Separate metadata tracing covered all 354 new trace files, excluded decoded private buffers and reported no Internet-family bind/connect. This does not replace missing marker coverage, prove universal non-disclosure or establish erasure.

The producer used disposable private environment/state and bounded process and sampled resource limits. Recorded peaks are samples, not aggregate continuous quotas. Actual long expiry, SettingsAgent/remote principals, injected caller-drop/panic/crash/races, Apple UI/OS, vault behavior, inference, all-feature runtime and production security remain unverified. Source/build evidence, manual runtime evidence, security review and deferred automated criteria are separate. No tests or fixtures were added; deferred automation remains [#32](https://github.com/izyuumi/everything/issues/32).

| Retained external receipt | SHA-256 |
| --- | --- |
| Producer handoff | `28b955c404276cea66ba0c175344878e17cd5ec65579b40de5a03446f026094a` |
| First immutable-audit handoff | `c4e05c3298695faaaeb7988e8da359e2e8a336174ff816ecea26052b1ff39350` |
| Independent review | `c1091d6c000957a0ba95fe39003ac2ab1c8fe7e45d7a4e079af299a40305b72c` |

These receipts identify retained external evidence; private runtime state and scripts are not repository artifacts. See the [device contract](../contracts/device-settings.md), [native routes](../contracts/native-settings-routes.md) and [earlier Settings observations](settings-validation.md) for the implemented boundaries and separately dated evidence.

## QR pairing walkthrough, 2026-11-01

This dated record covers the [`vorra pair` flow](../contracts/device-pairing.md) ([#270](https://github.com/izyuumi/everything/issues/270), [#271](https://github.com/izyuumi/everything/issues/271)) on macOS with a debug build of branch `feat/pairing-ux`. Everything used disposable state: a temporary `HOME` and `VORRA_STATE_DIR`, a throwaway host (`vorra serve`) and its own relay (`vorra relay serve --bind 127.0.0.1:18797 --origin ws://127.0.0.1:18797`) with a temporary database, and a throwaway iPhone 18 Pro simulator (iOS 27.0) running the VorraIOS Debug build, deleted afterwards. The owner's live host, relay, launchd service and tunnel were not touched.

The host was enrolled with `vorra relay enroll --keys HOST.json --pairing-host`. Before that, its first relay authentication failed and the relay pump stopped until the host restarted, which is existing behavior for an unenrolled host. `vorra pair` ran in a pseudo-terminal with its output captured; answers were typed into that terminal.

| Path | Host side | Phone side |
| --- | --- | --- |
| QR | An 81-column half-block QR and link were printed. The captured QR, rendered to PNG and decoded with `zbarimg`, equals the printed `vorra://pair#…` link (343 characters). | — |
| Approve | `<device> wants to pair  code 1249 7932  device 81a0d448-4f654215`, `y`, `paired: vorra-pair-throwaway`, exit 0. The host pinned the device with all-conversation, submit and resolve access; the relay listed it with `sponsor` = the host. | `xcrun simctl openurl` with the scheme link, system "Open in Vorra" confirmed, then "Approve on your Mac" with `1249 7932`, then "Paired with Throwaway Mac". Settings showed the relay origin and Connected through the sponsored enrollment. |
| Already paired | — | Opening a fresh link on the paired phone: "This iPhone is already paired with Throwaway Mac." |
| Refresh | With `--lifetime-seconds 30`, five distinct QR/link pairs were printed with "That code expired. Here is a new one." | — |
| `https://vorra.yumi.to/pair#…` | — | `simctl openurl` opened Safari (domain not live, so no universal link). Pasting the https form into the app's Paste button reached "Approve on your Mac" and the host prompt. |
| Reject | `n`, `rejected: vorra-pair-throwaway`, `vorra: pairing rejected`, exit 1. | "Pairing was rejected on the Mac." |
| Timeout | No answer within the 20-second code: the prompt timed out, a new QR was printed and the old code was withdrawn. | "This pairing code has expired or was already used." |
| Ctrl+C after a timed-out prompt | SIGINT: `vorra: pairing stopped; the code shown is no longer valid`, process exited. The first attempt hung on a blocked stdin read; stdin now uses a detached thread. | — |
| Relay down | — | "Your Mac could not be reached through its relay." |

A version 1 relay database written by the installed `vorra relay enroll` was migrated to version 2 by the new `enroll --pairing-host` (`user_version` 2, `pairing_host` 1); the old binary then refused it as unsupported.

Not exercised: a real iPhone, the stock Camera app scanning the terminal QR, the universal link (needs the live domain and association file), the in-app camera scanner (the simulator has no camera), two phones racing on one code, `pairing_in_use` and `pairing_unavailable` replies, an older relay without pairing support, TLS relays and cloudflared, the TUI onboarding pairing step, host restart during a pending request, and the Mac app (which still uses its older ceremony, [#272](https://github.com/izyuumi/everything/issues/272)). Screenshots and terminal captures stay outside the repository because the captures contain throwaway invitations. No tests were added; deferred coverage remains [#32](https://github.com/izyuumi/everything/issues/32).

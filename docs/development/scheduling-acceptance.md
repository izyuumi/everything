# Scheduling and continuation acceptance evidence

The 2026-10-16 observations cover calendar calculations, headless scheduling commands, native scheduling adapters and encrypted result synchronization on disposable data. Each observation retains its own artifact identity, process records and independent review. GitHub issues remain authoritative for acceptance decisions; this page records what actually ran.

## Source and artifact boundary

These observations use copied Rust 1.88 all-feature core `dbee29aec2d72795eac37c6abffbeb44b8ee6e04`, tree `58bb9e9e87939c17fa23f2e69513becdc25c9137`, with Store schema 12:

| Artifact | SHA-256 |
| --- | --- |
| CLI | `79adadbd3323c977ddc3804193cb5d6654789eaa18350d8c566cb1e5e5147f1f` |
| Library | `48ea256bf61ee98f88247fd47f47f8bf21d469de9ffd335a29a751db4d7bd0f9` |
| Relay | `14f2ab4f76d8116ed501edb41536fb54529c375b9aa33b13b9c3429a25734d0a` |

Independent source comparison found all 44 critical component entries identical to main `95f23a5548e48932ee58385da0c171d1475c1431`, including all 11 Scheduler and 11 Scheduling files. This establishes the recorded component identity. The newer Host settings/startup/owner/local transport and schema 13 have separate implementation and verification. Executing the copied core does not execute that newer whole Host or its private native credential transport.

The native-adapter observer was linked with opt-level 0 against the unchanged optimized production library. Its first opt-level 2 compiler invocation was killed with unconfirmed cause and remains preserved. The coordinator authorized one fresh link with changed output/environment/limits and opt-level 0, then separately authorized one runtime. All 430 enumerated compiler/source/sysroot/dependency/native inputs matched before/after the successful link. No producer source or assertion changed; no benchmark comparison follows from these timings.

## Calendar and due dispatch

The unchanged calendar observer passed 29 printed fixed-time expectations, plus overflow/year-domain/lower-bound and cancellation checks. Cases include New York gaps/folds, Lord Howe half-hour changes, Troll two-hour folds, Apia's skipped day, leap day, POSIX Sunday aliases and day-of-month/day-of-week behavior, subseconds, one-shot exhaustion and anchored intervals. Impossible calendar calculation returned Blocked. These are real library calculations over supplied timestamps; they do not change the operating-system clock or exercise timer dispatch.

The separate actual CLI sequence exercised automatic once dispatch and graceful Host reopen through the existing Engine/native Scheduler. The [engine contract](../contracts/scheduling-engine.md) describes its realtime audit, missed-run planning and atomic due advancement. This observation does not repeat the broader historical Engine missed-range/edit/rollback sequence or verify actual OS clock adjustment, every crash boundary or caller-drop timing.

## Headless CLI and scheduled accounting

The unchanged retained CLI walkthrough completed 113 recorded steps across all 10 commands: create, list, show, update, disable, delete, run, history, status and acknowledge-unknown. It exercised invalid cron/timezone/input bounds, bounded job/history cursors, revision conflicts, soft deletion, disabled manual execution and exact immutable replay after a workspace edit. Automatic once execution appended a scheduled-result event while the destination retained 0 model messages and no pending turn.

An earlier wrapper attempt reached 17 parser checks, then actual Host startup rejected its 106-byte socket path against the portable 100-byte bound. That failure was retained. The coordinator authorized one fresh shorter private path, with the same producer and assertions; the completed run used a 59-byte socket path. This was an explicit environment correction, without discarding the first outcome.

Five genuine unavailable-credential factory boundaries retained NoAttempt terminal evidence with 0 native charges. One public disposable session value led to an actual native connection refusal at an owned non-listening literal-loopback destination. Its original terminal evidence retained 1 model request, 128 reserved output tokens, 0 tools and scheduling Unknown. Wrong-digest and changed-time acknowledgements failed; exact owner acknowledgement replay preserved the original receipt and released only scheduling overlap. Graceful reopen retained Unknown, accounting and acknowledgement without another IP connect. Five separate factory failures do not verify a single three-attempt retry/backoff sequence.

Both actual Host processes exited 0; all 735 recorded process/thread/tracer IDs were absent. Three 0600 databases passed integrity and foreign keys at schemas 12/2/1. Ordinary read-only inspection created WAL/SHM sidecars for all 3 databases, with before/during/after inventories recording those effects; only the main database bytes were unchanged. This is not an immutable or unchanged-footprint audit.

The copied CLI used its legacy credential-reference owner route with a public disposable value. This verifies the stated scheduling boundaries, not current native secret-channel safety, OS credential persistence or successful inference. See [execution policies](../contracts/scheduled-execution.md) and the [owner CLI](scheduling-cli.md).

## Native scheduling adapters

The byte-unchanged 1304-line observer, source SHA `66c5d1abcf95a660935a55b9625ce689f34e57b54301fd7852ff332ae60ff644`, ran once as artifact `dbd25f8276c0ca478c03328287885d86c0842eb0147be3ef898bbda4b069b150`. It exited 0 in 4.807522 s with the final Agent/Store reopen message. Its own verification JSON is written before reopening, so final stdout and exit were also checked.

Actual Broker/Authorizer operations covered all 8 [native schedule controls](../contracts/scheduling-controls.md#native-tools), exact Host approval, ReadOnly denial, closed schemas, run-capability rejection, owner CRUD/calendar/CAS, captured target authority and immutable manual replay across workspace/destination changes. An agent could not adopt a foreign owner target or configuration. Closing mutation intake preserved owner inspection; actual service drains and reopen completed.

| Observed projection | Result |
| --- | --- |
| Owner jobs and pages | 114 jobs, 8 pages |
| Scoped native jobs and pages | 42 jobs, 22 pages |
| Inspected immutable history | 30 entries, 3 pages |
| Durable manual occurrences | 33: 3 Claimed and 30 Skipped |
| Scheduled native attempts/results | 0/0; Engine remained inactive |
| Tool journals | 36: 30 Succeeded, 3 Failed, 1 Denied, 2 Cancelled |
| Final Running/Unknown tool journals | 0/0 |

The 2 Cancelled journals are explicitly supplied Pending/Running copies of a genuine Broker show request, used to inspect exact context identity, state revalidation and worker lifetime. They are not model-emitted operations or proof of external effects. Manual occurrences establish actual auditable claims/overlap skips. Two delivery reservation rows do not establish result publication or scheduled inference.

The wrapper retained its own TCP socket bound to 127.0.0.1: 9 without listen, reuse or inheritance. Releasing the actual native provider-construction gate caused one asynchronous connect, then SO_ERROR ECONNREFUSED, and invalidated the retained caller context. The original native caller remained Waiting/provider_uncertain with 1 attempt, 128 reserved output tokens, 0 tools and private native ProviderInFlight. No automatic retry, refund or completed effect is implied.

Five metadata trace files total 156577 bytes; their sole Internet-family connect was that owned loopback refusal. The six send/receive families were configured as raw metadata and made 0 calls; no decoded send/receive/read/write buffers were captured. All 7 recorded wrapper/tracer/thread IDs were absent, with no forced cleanup. Process absence does not establish a universal internal thread-join guarantee. Runtime limits were per process, with separately monitored wall/disk/log bounds, rather than hard aggregate quotas.

After all joins, the producer and an independent reviewer separately opened the single ordinary 0600 database using immutable/query_only, having verified absent WAL. Actual schema 12, integrity and foreign keys passed. All 22 bounded metadata query results agreed. Complete file/DB/sidecar mode, ownership, link, size and hash inventories were identical before/during/after; no sidecar or reader error occurred. The qualified independent review is receipt `511c07b0c14f71901b7e7c191d27d658998313aa8c1d72ad23c70390b8b92770`.

This directly exercises real adapters and their caller restrictions. Successful model-origin tool selection, active scheduled execution, owner IPC and the CLI are separate observations.

## Encrypted scheduled-result delivery

One approved V2 walkthrough completed 27 actual CLI commands in 4.225797 s with real disposable Rust Endpoint/Replica identities and the existing blind relay. Two owner-created manual occurrences ended at genuine unavailable-credential factories with known 0 model requests/tools/output reservations. Exact owner fingerprint approval selected 2 conversations. After offline delivery, reconnect matched event envelopes/IDs/cursors and durable application acknowledgements; duplicate sync and graceful Host restart preserved them. The destination retained 0 model messages and an unchanged private checkpoint. See [scheduled results](../contracts/scheduled-results.md).

The real relay queued a 1715-byte encrypted envelope. The retained sequence checked absence of 3 public plaintext markers; that observation is not a cryptographic audit. Denying an unselected conversation exercised the local Replica scope check, not a hostile encrypted request reaching the Host. Apple UI/device interoperability and background notifications were not exercised.

V1 was never executed. V2 changed only metadata destinations/identity and suppression of Owner stdout/stderr logs, preserving the existing functional assertions. Its 52 Owner log files contain fixed omission markers; successful response JSON was consumed in RAM. The legacy temporary OwnerApprovePairing request carried PairingRequest proof/signature metadata. This old owner route does not verify current private Native Settings transport or secret erasure.

All 5 actual processes exited 0; all 235 recorded IDs were absent. The helper joined its relay task, while its stdin-reading thread has no separate JoinHandle. Metadata traces had 624 raw send/receive calls with 0 decoded payload lines; all 7 IP connects targeted the owned loopback relay. Five 0600 databases passed integrity/FKs at schemas 12/12/1/2/1. Ordinary read-only inspection had explicitly recorded WAL/SHM effects. A mistaken nested EventEnvelope reader projection was corrected without SQL writes or repeating the producer; the original failed read was retained.

## Continuation and composition handoff

The separately published [Scheduler observation](scheduler.md) used the same copied core with a root and 3 children making 4 concurrent held HTTP 500 requests. Responsive controls, real mid-request steering/cancellation, queued steering across reopen and one exact explicit owner retry were observed. Provider uncertainty persisted. The [steering contract](../contracts/steering.md) retains the earlier held 503 and separately supplied child/final-phase qualifications. Successful continuation, every child-completion/final-response race, saturated shutdown and caller-drop timing are not established by these observations.

For [#68](https://github.com/izyuumi/everything/issues/68), the existing headless composition reuses one Agent/Scheduler/Store/lease/Broker; its Wiki/FTS, MCP, explicit selector and embedding observations have separate artifact records in [the composition contract](../contracts/host-composition.md). This scheduling evidence does not re-execute newer Host MCP/selector/memory startup. The latest hybrid extension still assigns the hard restricted Settings profile, seven tools and memory/selector exclusion to [#113](https://github.com/izyuumi/everything/issues/113). Native actions/UI/explicit remote grants remain their separate issues. Optional Apple UI delivery does not gate base headless composition.

## Retained receipt identities

These are local evidence locations retained outside Git; generated databases, raw logs and disposable state are not repository files. Each entry's qualification remains in its original observation/review.

| Phase | Retained receipt | SHA-256 |
| --- | --- | --- |
| Calendar | `/workspace/current-core-calendar-acceptance-20261016T1030/verification.json` | `704f69154e9ad654906e1cfa2e97176a6310ab8ae23045fe2c4626d9de4a6b0a` |
| Calendar independent review | Same directory, `independent-memory-review.json` | `ae125e205c8d5486a794507421c0994d8cbe64a672a23156b4b764bf6a1847a0` |
| CLI | `/workspace/sc53b/verification.json` | `462afcf91094ce7bfef2993e869c5473fefa2892694545091c445a3dcbe98483` |
| CLI independent review | `/workspace/sc53b/independent-memory-review.json` | `bb7eeb577a41615845a640127fc57384f37647d64c7402448a60c7911aae2caf` |
| Native adapters | `/workspace/s53n.e2/verification.json` | `75e2093cd33a933153edf1edfdd8fe49dc73f0348f0b946f59d363e3916ecdc6` |
| Native independent review | `/workspace/current-core-schedule-native-independent-review.json` | `511c07b0c14f71901b7e7c191d27d658998313aa8c1d72ad23c70390b8b92770` |
| Encrypted delivery V2 | `/workspace/s54x/verification.json` | `30e8ca71367e18b3dd5586ccfaae3c818b1be329287ec047b0c4ed185a68f0fd` |
| Encrypted delivery independent review | `/workspace/s54x/independent-memory-review.json` | `116216dbf9b91aa7f32f2a1377928710b1d4e1757074e30bb222e07dbacde711` |

## Remaining validation

The specific current-copy multi-attempt backoff, broader Engine edit/missed-run/rollback sequence, terminal-evidence reentry/foreign-summary/partial-capacity and crash/caller-drop cases remain historical, source-reviewed or unobserved as described in their contracts. The 1024-byte Scheduler terminal-evidence reserve is a checkpoint-byte bound; it is distinct from the 128 output tokens observed in refused requests.

Successful conversational/classifier inference and model-generated maintenance/tool choice, OS credential persistence, actual OS clock adjustment, Apple UI/device/launchd and security review remain unverified here. SDK compilation and static analysis do not establish those runtime observations. Automated acceptance remains deferred in [#32](https://github.com/izyuumi/everything/issues/32); no new tests or fixtures were created. Required source/documentation CI and normal review/merge remain separate integration gates.

# Revisioned Settings validation

The integrated Rust checkpoint `dadceaa7e212c4499269bbc7be3d39e872d91bec` passed Rust 1.88 formatting, locked default/all-feature all-target checks, both strict Clippy forms and both optimized workspace builds. The walkthrough below uses that exact source and its copied artifacts. Later commit `e044b5c` only collapses two existing pairing-journal/owner-receipt validation guards for stable Rust 1.99 Clippy, with independently reviewed equivalent conditions. Normally merged PR #131 separately adds synchronous native notification admission. These later changes require their own exact-source checks; they do not change the identity of the older runtime evidence. Local gates do not replace required PR CI or supported Apple SDK validation.

The following disposable Linux walkthrough used fresh schema-13 state and the actual copied all-feature release artifacts. It reused the retained Host/trace helper with only artifact/output path projections; the new interaction ordering was a manual stdin transcript. No new automated tests, fixtures or production substitutes were introduced.

| Artifact | SHA-256 |
| --- | --- |
| Host/CLI | `32bcc23740c5d36f90ada20defa35e3eef81eecbae0381a0d59626227af1d6c2` |
| Adjacent embedding worker | `9e1dacb0ac2105d052fad7ab3b252e91192a3bf65ffa009c61ec004a18d40fcf` |
| Library | `8e0c50935e5006c5dd7b2c6b9d8506b8f09fb5f05c9b4ab6e9bb8ab157d22b6a` |

## Actual local and native operations

One fresh state supplied three actual Host starts and successful drains, finishing at settings revision 21. The owner CLI and private FIFO native route observed the same authoritative revisions. A typed off patch saved once; exact replay retained its original pending-file outcome and later separate application observation. Changed-payload reuse and a stale owner edit were rejected. The stale edit returned a CLI error rather than the receipt initially expected by the observer; its original output was retained. A general owner save and its exact replay preserved the same revision. After later saves and a Host restart, that old owner receipt remained inspectable and returned no substituted newer settings snapshot.

A raw owner model change returned HostActionRequired without saving. A separate native patch produced the exact retained confirmation, and its native confirmation saved once. System One configuration and off each acquired their own confirmation/application result; off preserved the configured endpoint/model. No conversational or classifier request was made by this walkthrough.

An explicitly configured Session credential binding accepted a generated disposable value through the actual private pipe and vault map, producing one original Completed receipt. Replay returned that inspection without changing settings. This verifies the exercised configured Session path, not first-time enrollment, SecureField presentation, Keychain or Secret Service. The value was never supplied to a provider.

Device listing returned the existing redacted projection. Native confirmation created an actual pending invitation; cancellation removed its pending use, and confirmation replay returned the existing operation without another invitation. The device list stayed unchanged: invitation creation was not pairing or approval. Final pairing approval, access selection and exact-peer revocation were not exercised here.

A native section action was admitted and explicitly dismissed, returning Cancelled. Wrong private proof was rejected. Closing the native session retained original cleanup inspection/reconciliation and rejected fresh update admission. Host restart rejected an old native proof. Owner original-ID inspection and reconciliation stayed distinct from file rewriting or application. These observations do not exercise a retained-lease Host drain with unresolved effects; that boundary retains its source checks and separate validation requirements.

## Real live embeddings and file/application outcomes

The public orchard Wiki was created through the existing explicit maintenance-plan operation. Its content hash remained unchanged through the embedding sequence. Operator configuration was saved while an explicit off override remained set; no helper was created and retrieval stayed lexical. Descriptor dimension choices were absent before a real profile was known.

Actual live application then produced hybrid retrieval at 128 dimensions, configuration-preserving off with the old helper gone, a 256 choice while off without a helper, enabled 256, live 512, and the backend-default choice with actual 768-dimensional execution. Each application retained its original operation and file revision. The worker used the production strict Linux boundary, and point observations checked distinct helper network namespaces, cleared environment, no inherited sockets and absence of exposed Host roots. Old recorded helper/monitor PIDs were absent after their observed replacement/off boundaries. These are point observations, not continuous native-entry or maps instrumentation.

Temporarily removing only the disposable adjacent helper caused a genuine unavailable application and lexical semantic-failure fallback. Returning the same helper file and replaying the original request did not respawn it. A fresh off operation settled normally. Persisted off then survived a real Host restart with configuration retained and lexical retrieval.

The third start exercised a real concurrent file/application boundary. An enable saved revision 18; an independent owner general edit saved revision 19 while that application was pending. The original application settled Failed/apply_failed because its exact post-file source no longer matched. Neither a generic ready backend nor the newer file was substituted into its original receipt. A later off saved revision 20. A non-embedding Memory policy edit saved revision 21 and returned actual RestartRequired with the composition's memory restart flag set. No crash or uncertain-file outcome was fabricated to obtain these states.

## Sensitive transport and evidence limits

Actual CLI observations rejected regular-file input/output and 32,769 input bytes with zero response bytes. A missing EOF ended after 15.019 seconds with zero response bytes; SIGINT while awaiting input also ended without a response. These negative invocations ran directly after Host joins, carried no private proof or credential, and were not straced.

After all three Host joins, the producer scanned the complete disposable job files for the exact three private proof values, Session credential and invitation value retained only in its memory. None appeared. This is an exact-marker observation, not universal Swift/Rust buffer erasure or a security audit. Raw sensitive requests/responses and marker values were not persisted as evidence. The independent audit did not request those private values or reproduce that scan.

Independent retained-evidence review matched all 55 recorded inspections to the actual immutable history: 24 terminal operations, 71 updates, six native claims and 20 exact file-fingerprint transitions through revision 21. Separate application observations comprise thirteen Applied, one Unavailable and one Failed. Bounded vector retention leaves only the final 512/768 generations, each ready with two newly embedded vectors and zero reuse; the earlier 128/256 evidence is the live transcript, not four final retained tables.

All four private databases were inventoried before immutable/query_only reads at schema versions 13/2/1/1. No WAL/SHM existed, and the complete path/mode/size/hash inventory matched afterward. Host, lexical and maintenance integrity/FK checks passed; the vector audit read ordinary metadata without loading an extension or scanning virtual-vector data, and makes no vector-integrity claim. All 2,520 retained traces, totaling 6,011,640 bytes, were hashed; no Internet-family observation appeared and every recorded trace PID was absent. The complete configured-off second Host trace contained no helper execution or configured artifact/vector-path access. Copied binaries and all five pinned public model/runtime files matched their manifests. The independent review receipt SHA-256 is `73dfb9c62a0f611b2e7e030f061407d27bf91938963efb73aabf898a6576803c`.

The walkthrough does not establish successful model/classifier inference, the restricted Settings model runtime, active-task provider/context capture, Apple UI/OS/Keychain behavior, different-UID rejection, unknown-file crash repair, every cancellation race or security readiness. The plans supplied to Wiki maintenance were explicit observer input, not model output. Automated verification remains deferred to [#32](https://github.com/izyuumi/everything/issues/32).

See the [service](../contracts/settings-service.md), [startup](../contracts/host-settings-startup.md), [owner receipts](../contracts/settings-owner-routes.md), [native routes](../contracts/native-settings-routes.md) and [live memory controls](../architecture/memory-live-application.md).

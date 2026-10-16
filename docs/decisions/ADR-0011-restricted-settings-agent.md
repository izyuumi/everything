# ADR-0011: Restricted Settings uses the same kernel and a private state view

Status: Accepted design; source integration and combined validation in progress.

## Context

[#113](https://github.com/izyuumi/everything/issues/113) requires an on-demand Settings agent with exactly seven typed tools, no ordinary chat/memory/tool authority, pinned native model disclosure and safe ownership during cancellation or client loss. A different prompt cannot enforce those boundaries. Another runtime, Broker or credential gateway would duplicate execution and cleanup semantics.

## Decision

Clone the actual leased Agent, preserve its maps and StateLease, and select a bounded fileless SQLite Store for private Settings execution. Reuse its native loop and the Host's real Broker/registry/permissions infrastructure with opaque crate-private profile contexts and seven private entries. Construct the existing General selector catalog before those private adapters are registered, avoiding a service/selector ownership cycle.

Keep raw chat, continuation, arguments and read results in bounded RAM. Reserve immutable namespace identities and coarse monotonic execution fences in the ordinary Store with atomic both-direction SQL exclusion. Markers do not grant authority or reconstruct lost RAM. Existing typed Settings operations remain owned by the same ServiceCore and its native confirmation workflow.

Capture one native provider from one source snapshot and require explicit acceptance of its exact recipient/model binding. Serialize canonical source revalidation and original pre-stream accounting/private InFlight save with the actual Host mutation guard, retained through the real blocking worker. Preserve actual usage and distinguish provider resources, worker quiescence and effect disposition. Lost or uncertain work is never automatically retried or refunded.

Retire provider-only locally drained interruptions/unusable responses with distinct factual terminal metadata. Genuine unresolved work quarantines the same Broker. Recovery requires actual owner authentication, drain witnesses and a separate immutable exact-fence acknowledgement that preserves original uncertainty and never fabricates ordinary journal rows.

## Consequences

Native controls and Host lifetime remain independent of the optional chat/UI and provider availability. A closed or lost-RAM chat cannot resume. Unknown work consumes bounded live/closing capacity until reconciliation; lifetime tombstones are bounded and never silently evicted. SQLite page and encoded-value ceilings do not bound total process RSS. The design still requires exact-source compilation, disposable failure/privacy validation, supported Apple verification and later automated/security review before broader readiness claims.

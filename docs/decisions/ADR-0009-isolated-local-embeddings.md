# ADR-0009: Isolate production local embeddings

Status: accepted; focused Linux x86_64 production-boundary observations passed.

## Context

Issue #72 observed native runtime telemetry despite API suppression. A process-start flag controls one trusted runtime but cannot enforce local-only execution. A process-wide network ban would disable approved native model and relay services.

## Decision

The Host uses an embedding-only helper launched through a strict established Linux bubblewrap boundary. Only that child loads EmbeddingGemma 2 and its native runtime. The existing synchronous embedding/profile contracts cross bounded private pipes; Wiki and vector SQLite ownership remain in the parent. The Host performs an artifact-free confinement probe first, fails closed to explicit unavailable status and genuine FTS when no supported boundary exists, and never falls back to in-process native inference. Telemetry suppression remains defense in depth.

The memory supervisor retains the helper and Agent lease through blocking shutdown and actual reap. Unsupported platforms retain lexical retrieval until their own enforced boundary is implemented and verified.

## Consequences

Opt-in Linux inference requires the helper installed adjacent to the Host, bubblewrap and permitted required namespaces. Provider/relay networking remains independent. The public low-level native adapter and public benchmark remain useful for trusted development measurements but do not provide production confinement. No additional model, remote embedding service or Python daemon is introduced.

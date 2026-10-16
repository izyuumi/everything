# ADR-0009: Isolate production local embeddings

Status: accepted and implemented for the production Linux GNU boundary. Qualified Linux x86_64 core observations are recorded below; unsupported platforms retain FTS and automated acceptance remains deferred to #32.

## Context

Issue #72 observed native runtime telemetry despite API suppression. A process-start flag controls one trusted runtime but cannot enforce local-only execution. A process-wide network ban would disable approved native model and relay services.

## Decision

The Host uses an embedding-only helper launched through a strict established Linux bubblewrap boundary. Only that child loads EmbeddingGemma 2 and its native runtime. The existing synchronous embedding/profile contracts cross bounded private pipes; Wiki and vector SQLite ownership remain in the parent. The Host performs an artifact-free confinement probe first, fails closed to explicit unavailable status and genuine FTS when no supported boundary exists, and never falls back to in-process native inference. Telemetry suppression remains defense in depth.

The memory supervisor retains the helper and Agent lease through blocking shutdown and actual reap. Unsupported platforms retain lexical retrieval until their own enforced boundary is implemented and verified.

## Consequences

Opt-in Linux inference requires the helper installed adjacent to the Host, bubblewrap and permitted required namespaces. Provider/relay networking remains independent. The public low-level native adapter and public benchmark remain useful for trusted development measurements but do not provide production confinement. No additional model, remote embedding service or Python daemon is introduced.

The actual Host integration retains Wiki admission, native/Scheduler ownership, task-specific terminal accounting and schedule controls. On 2026-10-16, copied normal-package core artifacts from `dbee29aec2d72795eac37c6abffbeb44b8ee6e04` performed real helper-owned rebuild/hybrid retrieval at 128/256/512/768 dimensions, fail-closed FTS checks, stale-source rebuilding and owned-helper failure without respawn. Separate unchanged external inspectors linked to that exact core library exercised cancellation, deadlines and dropped-owner cleanup. Their scope and provenance are recorded in [the composition checkpoint](../contracts/host-composition.md#current-core-embedding-evidence) and [the observation guide](../development/embedding-benchmark.md#current-core-production-boundary-observations). A separate same-database `dbee29a` walkthrough later verified persisted 128 restart, a 128→256 rebuild boundary and Disabled plus restart; it does not exercise the new PR #129 live/generation controls. Current memory code has changed, so historical whole-memory source parity must not be extended to those controls. Their public build/static CI passed, while hook/runtime integration remains separate. Successful provider inference, Apple confinement and general security remain unverified. No hard PID/RAM quota or arbitrary native-library certification is claimed.

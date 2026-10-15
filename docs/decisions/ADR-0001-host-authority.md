# ADR-0001: Host authority and local-first boundaries

Status: Accepted

## Context

The application must work without graphical clients or remote infrastructure. Remote devices and optional workers cannot become authorities for local execution.

## Decision

The Rust host owns durable events, permissions, tasks, and execution. Markdown remains canonical memory. Optional relay infrastructure handles ciphertext only. Provider adapters use native APIs. Cloud delegation requires explicit scoped capabilities. No backup subsystem or other-harness compatibility is introduced.

## Consequences

Clients submit commands and synchronize host events. A UI exiting cannot stop the runtime. SQLite indexes and derived views can be rebuilt from their authoritative sources. Network features remain optional. Implementation proceeds behind reviewed contracts.

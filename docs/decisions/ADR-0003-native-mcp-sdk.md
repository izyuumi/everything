# ADR-0003: Current native MCP SDK and Rust 1.88

Status: Accepted for issue #19.

## Context

Vorra requires native MCP tools with host-owned process lifecycle, broker permissions, cancellation and bounded resource consumption. The official Rust SDK's current stable release, `rmcp 3.5.1`, declares Rust 1.88. The bootstrap workspace originally declared Rust 1.85. Older releases have missing MSRV metadata or client-only feature defects; preserving that initial compiler choice would require an older SDK and extra server/macro dependencies. HTTP transport and OAuth are unnecessary for the initial configured stdio adapter.

## Decision

Use pinned official `rmcp = 3.5.1` with default features disabled and only `client` plus `transport-async-rw`. Raise the workspace's declared minimum Rust version to 1.88 and validate the locked dependency resolution on that compiler. Keep Vorra's runtime and model providers native. Use the SDK's protocol types and bounded codec inside an owned stdio/process transport because its default stdio reader does not enforce the required frame bounds. Do not enable server, macro, HTTP or authentication frameworks for this scope.

## Consequences

Contributors need Rust 1.88 or newer. The repository must validate its declared compiler floor and update setup/CI metadata with integration. A disposable external dependency build verified the chosen SDK and locked transitives on actual Rust 1.88.0 before implementation; the implemented workspace then passed `cargo +1.88.0 check --workspace --all-targets --locked`. The changed compiler declaration activates existing Clippy recommendations in other modules that integration must resolve. Apple-platform validation remains separate. Explicit local process trust, credential selection, immutable catalogs, broker approvals, cancellation and resource limits remain Vorra responsibilities. Remote MCP transports can be considered independently later.

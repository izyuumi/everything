# Host MCP lifecycle

`host::mcp` is the host-owned composition leaf for configured native stdio MCP servers. It calls the existing `tools::mcp` implementation and uses the existing Agent, registry and Tool Broker contracts. The headless Host integrates its settings, owner status and final shared registry through [service composition](../contracts/host-composition.md).

## Configuration and trust

`McpSettings` defaults to an empty server list. Starting it creates no process, performs no environment credential lookup and opens no network connection. Up to sixteen uniquely named servers fit within a 48-KiB serialized configuration budget; the enclosing host settings retain their own aggregate limit.

A server manifest contains an absolute executable, absolute working directory, ordered non-secret arguments, selected environment references and bounded deadlines. Its SHA-256 identity includes every manifest field and excludes only its launch authorization. `McpLaunchAuthorization::FullHost` must contain that exact manifest fingerprint. An absent authorization leaves the server awaiting authorization without launching it or resolving credentials. A stale authorization rejects the settings before startup.

Writing that authorization is an explicit local operator trust decision. The process starts before a model tool request and can exercise the host user's operating system privileges. Neither the fingerprint, the MCP server's advertised filesystem roots, process groups nor Tool Broker path checks authenticate a program or create an operating system sandbox. Use executables, working directories and script/configuration argument paths under trusted owner-controlled directories. Executables and non-secret arguments remain owner-managed files and values; changing a referenced executable's contents is outside manifest fingerprinting. Canonicalization does not authenticate a binary or eliminate path replacement races.

Environment configuration contains references, never literal values. Each `McpEnvironmentBinding` binds the server ID, whole manifest fingerprint, exact variable name, non-nil reference UUID and `session` or `native` storage. Its native credential namespace is `ai.everything.mcp.environment`. The caller supplies an actual `McpEnvironmentResolver`; the host vault adapter must use the complete binding. There is no fallback to ambient environment variables or a model-provider key. Resolution is cancellable and bounded by three seconds per lookup and the server's overall startup deadline. Resolved values are UTF-8, NUL-free, aggregate bounded and held in zeroizing owners before passing into the native launch configuration. The native MCP implementation clears the child environment.

Configuration changes require a host restart in this initial composition. Replacing the configuration does not mutate a running task's frozen catalog, authority or context.

## Registry and permissions

`McpService::start` starts its supervised lifecycle with at most four concurrent connection attempts; `ready` waits for those initial attempts. An unavailable optional server produces a bounded reason code while preserving other servers. Status contains server IDs, manifest fingerprints, actual process IDs and bounded native tool names, with no secret values, stderr text or raw protocol errors.

`register` uses each connected session's actual catalog on a blocking worker and returns the resulting registry plus its real `ToolScopeRule::Host` entries. The coordinator must install these scopes into the same selector and Authorizer before the existing global Tool Broker is constructed. Imported tools keep the existing conservative MCP capability, exclusive execution and FullHost exact-approval requirements. A stale or disconnected session never silently reconnects or replaces an active registry; status reports its failure and restart requirement.

## Cancellation and cleanup

Construction requires an Agent opened with a state lease; a bare `Agent::from_store` is rejected before startup. The lifecycle worker retains that Agent clone and state lease. Cancelling the supplied token or dropping the final service handle requests actual session shutdown. Dropping a `ready`, `register` or `shutdown` caller future does not release the lifecycle worker's ownership. A caller that keeps a service handle remains responsible for explicitly cancelling or shutting it down.

`shutdown` requests cancellation and waits up to 25 seconds for a confirmed result. `ShutdownPending` means cleanup continues under the retained owner; it is not a clean stop. Shutdown awaits at most sixteen independent session cleanups concurrently. Actual failed session shutdowns remain visible and are retried once per second. The worker releases its Agent only after every started session reports confirmed cleanup.

The existing native `McpHost::connect` can report `Cleanup` before returning a session. This leaves no recoverable session handle in the public adapter. The composition records that condition permanently as unconfirmed and retains its Agent and state lease. There is currently no leaf operator reconciliation API for that unidentified cleanup. It must not be labelled stopped or bypassed automatically. Operator inspection and a separately reviewed recovery mechanism are required if it occurs.

The start token controls the whole MCP process lifetime. Give it a dedicated host-owned token rather than the early task-work cancellation token; use `ready`'s cancellation argument for the bootstrap wait and shut down or drop the retained handle when bootstrap is cancelled. Root host shutdown must keep owner inspection available, drain the Scheduler and global Tool Broker, await this service's actual cleanup and release state ownership only after every required cleanup proof succeeds. No service path acknowledges an unknown broker operation.

## Validation boundaries

Rust 1.88 formatting, workspace all-target/all-feature checking, strict Clippy and workspace all-feature release passed for this leaf. A temporary unique package identity avoided shared-target cross-worktree artifact ambiguity; manifest and lockfile changes were restored and excluded from the implementation.

Disposable Linux observations used the installed upstream `@modelcontextprotocol/server-filesystem@1.0.0`. They verified empty/default zero-server and zero-credential-resolution behavior, unapproved zero launch, changed-manifest rejection, rejecting an unleased Agent, exact variable-scoped credential binding, an actual fourteen-tool catalog, only the explicitly selected child environment entry, ReadOnly denial, FullHost exact approval, an actual file write and durable operation replay. Explicit shutdown removed the child; dropping the last service handle kept the state lease until process cleanup and successful state reopening. A missing executable reported degraded availability. All three observed state databases had mode 0600, clean foreign keys and successful SQLite integrity checks.

The environment resolver supplied a disposable public marker through the real typed binding interface; native credential-store persistence and the final host vault adapter were not verified here. The unidentified native cleanup failure path was reviewed but not induced. Automated coverage is deferred to issue #32. Native Apple execution, operating system containment and security audit require separate validation. No live model-provider success or complete headless composition is implied by these MCP observations.

The focused full-host walkthrough subsequently verified the actual catalog/permission/lifecycle path, final vault/bootstrap, and a real upstream write through the same global Broker from explicitly seeded pending dispatch. Model attempts remained zero, and restart did not repeat the write. The [composition contract](../contracts/host-composition.md) retains these qualifications and separate native-store/platform limits.

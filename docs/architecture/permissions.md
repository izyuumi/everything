# Permission policy

`src/permissions.rs` implements issue #11 as a synchronous policy service over the trusted tool registry and durable SQLite permission records. It returns allow, ask, or deny decisions; it never invokes tools. The Tool Broker must enforce those decisions before execution. Move policy calls to a blocking worker because they validate JSON Schema, resolve filesystem metadata, hash arguments, and read SQLite.

## Host authority and inheritance

`ExecutionAuthority::new(conversation_id, task_id, workspace_root, mode)` accepts host-supplied identity and mode. It canonicalizes an existing absolute UTF-8 workspace directory. Authority has private fields and no deserializer. Never construct it from model arguments, model tool metadata, or unverified client messages.

Modes form a restriction order: `ReadOnly` < `TrustedWorkspace` < `FullHost`. `child(task_id, mode)` inherits the parent's conversation, canonical workspace, and optional tool allowlist, and rejects a higher mode. `child_in_conversation(conversation_id, task_id, mode)` supports a separate child conversation while retaining those inherited restrictions. Both require a distinct task ID. `restrict_tools` can introduce or narrow a tool allowlist; it cannot widen an inherited allowlist. Grants bind their original conversation and task and are never implicitly inherited.

`ReadOnly` permits trusted local read-only tools and declared filesystem reads within the inherited workspace. It denies filesystem writes. `TrustedWorkspace` additionally permits declared filesystem writes within the workspace. Both modes deny shell, network, MCP, host-scoped tools, and filesystem targets outside the workspace. `FullHost` can request explicit approval for those operations. FullHost selection alone does not approve them. A shell descriptor's `read_only` flag and path arguments never confer shell permission.

## Trusted tool scopes

`Authorizer::new(Store, Arc<ToolRegistry>, BTreeMap<String, ToolScopeRule>)` validates host-owned scope registrations against the registry's immutable descriptors. A missing registration denies the tool. Scope rules are:

- `LocalReadOnly`: requires a read-only descriptor with no external capabilities.
- `Filesystem { path_pointers }`: identifies 1–16 trusted JSON Pointers containing individual paths or arrays of paths. The descriptor may declare only filesystem read/write capabilities. Each pointer contains 1–64 paths. Targets resolve against the inherited workspace. A missing final path is supported for filesystem-write tools by resolving the nearest existing directory. Unresolved special components and broken symlinks are rejected.
- `Host { service }`: binds the host-controlled service/server identity and always requires FullHost plus explicit approval. Register shell, network, and MCP tools this way; the full argument digest binds remote destinations and operation parameters.

`tool_visibility(authority, tool_name)` returns `Available`, `RequiresApproval`, or `Denied` for discovery. Available filesystem candidates still require argument-specific authorization before execution. Discovery never creates approval records or authorizes a side effect.

`authorize(authority, operation_id, tool_name, arguments, now)` validates arguments through the registry, validates the bounded active policy snapshot, resolves trusted target rules, and evaluates authority. The operation ID comes from the host journal. Model-supplied mode, workspace, grant, or capability fields have no authority. Decisions are `Allow { grant_id }`, `Ask { approval }`, and `Deny { reason }`.

## Exact scoped approvals

An approval request persists the exact validated arguments for host review and an `AuthorizationScope`: conversation, task, operation, canonical workspace, effective mode, tool name, descriptor capabilities, resolved targets, and SHA-256 argument digest. SHA-256 comes from the established `sha2` library. Deterministic approval IDs bind conversation and host operation ID. Reusing an operation with changed arguments, tool, task, mode, workspace, or targets conflicts with the original request.

Requests last at most 30 minutes. The host must present both exact arguments and scope when collecting approval. `Ask` is pending state and must never execute a tool. `pending_approvals` and `approval` reconstruct pending and terminal state after restart.

`resolve(approval_id, ApprovalResolution { choice, expires_at }, now)` writes one immutable resolution record. Allow and deny are explicit choices. Optional grant expiration can shorten the original deadline; it cannot extend it or change scope. The resolution row is the grant ID for approved operations. An identical resolution retry returns the persisted outcome; a conflicting retry fails. Resolution IDs are unique in SQLite, so concurrent conflicting choices cannot both win. An expired pending request cannot receive a fresh resolution.

Approved grants permit the original exact operation only. They cannot approve another task, conversation, workspace, tool, set of arguments, destination, or execution mode. A denial remains terminal for that operation. `revoke(grant_id, now)` revokes an approved resolution, supports idempotent retries, and prevents reapproval of the original operation. Revoked, denied, and expired outcomes remain reconstructable through exact record reads.

Permission rows contain a versioned, tagged approval or resolution record with strict field decoding. Unknown versions, unknown kinds, inconsistent scope digests, malformed records, and active snapshot exhaustion return errors; the broker must fail closed. The active snapshot is limited to 1000 records and the storage read-byte budget. Native row expiration matches request/grant expiration, so historical decisions stop consuming that active budget while remaining durably queryable. There is no wildcard grant or model-generated grant path.

## Execution and filesystem boundaries

An allow decision is a policy observation at the supplied time, not a persistent execution capability or operating-system sandbox. The broker must recheck authorization immediately before changing the durable journal from pending to running and invoking a tool, including after approval waits or exclusive-execution queue waits. A revocation racing an already-running operation cannot undo an external effect. Host cancellation and tool cleanup must handle running work explicitly; never replay an unknown tool outcome automatically.

Filesystem path resolution provides scope checking and review metadata. It does not eliminate symlink races or confine arbitrary shell execution. Native filesystem implementations must enforce workspace access at the actual I/O boundary using safe directory-relative handles or equivalent platform protection. Arbitrary shell execution requires FullHost consent regardless of its working directory. Permission checks never claim path checks sandbox a shell.

## Validation and limits

Compilation, formatting, Clippy, and disposable manual policy inspection passed. Inspection confirmed shell denial under ReadOnly despite a read-only descriptor and model-supplied mode/root fields, approval-required discovery, rejected child escalation/allowlist widening, durable pending requests, idempotent resolution, changed-argument conflicts, grant survival across reopen, idempotent revocation, expiration, and fail-closed unknown records. Release compilation passed. Automated policy, lifecycle, restart, concurrency, and inheritance assertions are deferred to [#32](https://github.com/izyuumi/everything/issues/32). Broker enforcement, interactive approval UI, cancellation of in-flight tools, and operating-system filesystem boundaries belong to their implementation issues. Local permission rows contain plaintext arguments needed for review and require the host's private filesystem permissions; they must not be logged or sent to optional cloud services without authorization.

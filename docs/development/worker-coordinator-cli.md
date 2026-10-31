# Worker coordinator owner CLI

`vorra worker coordinator` dispatches to `host::worker_commands` for issue [#29](https://github.com/izyuumi/everything/issues/29). Argument parsing, bounded owner-file input, actual owner IPC registration and the optional Host coordinator are composed in source. The six combined normal-package Rust 1.88 gates passed at `d273c52`; the actual production Host consent/native-admission/credential-rejection/recovery walkthrough also passed, while the earlier isolated leaf check alone establishes no live consumer behavior. Automated verification remains deferred to [#32](https://github.com/izyuumi/everything/issues/32).

The coordinator uses the ordinary running host's same-user owner socket. Start the actual configured host independently; these commands never open another Agent, start an executor, connect directly to a relay, load a model credential, run a task locally or create a second scheduling loop. A disabled or unavailable coordinator must report its actual state or reject unsupported actions. Executor commands such as `worker serve` use the separate dedicated-worker configuration and are not replaced by this surface.

## Inspect and pin purpose identity

```sh
vorra --state-dir STATE worker coordinator status
vorra --state-dir STATE worker coordinator identity
vorra --state-dir STATE worker coordinator peers
vorra --state-dir STATE worker coordinator pin-peer --file executor-public-keys.json --fingerprint FULL_EXECUTOR_FINGERPRINT --request-id REQUEST_UUID
```

Replace the capitalized values with the actual state path, full confirmed fingerprint and non-nil request UUID. The pin input is exactly the existing DevicePublicKeys JSON object, containing `device_id`, `signing_key` and `encryption_key`; it is not an identity-response wrapper and contains no private keys. Read the executor's independently confirmed complete fingerprint before granting purpose trust. The actual operation validates the complete keys and fingerprint. Ordinary device pairing does not grant worker trust, and this CLI never discovers a server or approves a pin automatically.

`status` returns the actual enabled/draining/admission/relay counters and fixed diagnostics. When no coordinator service was constructed, status performs no worker identity creation or ledger scan: retained `jobs` and `unresolved_remote_tasks` are null, while in-process preparations and operations are known zero. A retained service returns actual bounded inspected counts even after a live settings change disables it; `enabled: false` alone does not mean those counts are unknown. `identity` and `peers` require the retained service and return only public purpose keys and sticky trust metadata. Querying does not authorize a task or prove that disconnected remote work stopped.

## Prepare and review the full disclosure

```sh
vorra --state-dir STATE worker coordinator prepare --file preparation-request.json --request-id PREPARE_REQUEST_UUID
```

The input is the complete strict [WorkerPreparationRequest](../contracts/host-worker.md#settings-and-preparation): explicit executor, physical workspace, objective, constraints, selected context, ordered input paths, allowed output paths, mode, worker model/reference, ContextBudget, LoopLimits, deadline and tool set. All five context-budget fields are required, unknown fields are rejected, and `limits.absolute_deadline` must equal `deadline`. Use a current deadline rather than a copied expired timestamp. This is metadata input; the host's existing descriptor-based capture and WorkerManifest validation remain responsible for actual file scope and supported policies.

Preparation requests are limited to 256 KiB. The selected file must be owned by the current effective UID, regular, single-link and not group/other writable. The CLI opens its final component with NOFOLLOW, NONBLOCK and CLOEXEC and reads at most the limit plus one byte before rejecting growth. FIFO/device/directory inputs cannot wait for a writer. The same checks apply to public-key files, whose limit is 4 KiB. The path itself is bounded to 4096 UTF-8 bytes without control characters. Ancestor paths and the contents of selected task inputs remain the host's separate trust boundary.

The returned JSON is the complete validated owner reply, without truncating or hiding the preparation. Its preparation contains the ticket, original times, full WorkerManifest including base64 input contents, exact manifest digest, complete executor keys/fingerprint and the local task/conversation/command/policy plan. Inspect all selected content, provider destination, file bytes, tool scope, budgets and deadline before deciding. The stdout disclosure can contain private task data; handle it as private local data and do not commit it or include it in shared logs. The CLI does not print that payload in its stderr request record.

## Explicit delegation and discard

Copy the exact ticket, manifest digest, executor fingerprint and **local.create_command_id** from the reviewed preparation:

```sh
vorra --state-dir STATE worker coordinator delegate --ticket TICKET_UUID --manifest-digest MANIFEST_SHA256 --executor-fingerprint FULL_EXECUTOR_FINGERPRINT --request-id LOCAL_CREATE_COMMAND_UUID
vorra --state-dir STATE worker coordinator discard --ticket TICKET_UUID --request-id DISCARD_REQUEST_UUID
```

Delegate requires an explicit request ID; there is no generated default for that command. It must equal the disclosed `local.create_command_id`. The Host compares that ID and the complete original ticket/pin/policy against its retained live preparation before effects, then mints a thirty-second admission approval bounded by the manifest deadline. The CLI cannot recreate the opaque ticket or validate its current authority from a printed JSON object. It does not refetch preparation, renew expiry, approve a replacement or retry after rejection/disconnect. Ticket expiry is not a stored reusable approval.

A successful Admission-shaped reply describes the actual durable owner command receipt and worker ledger/native projection. Accepted, Applied, Completed and Rejected retain their existing meanings. Neither an Admission response nor ledger Offered proves that the executor started, that an Offer was transmitted or that remote effects completed. Inspect the actual job/task after an ambiguous response and preserve the original request and payload. A discarded/expired ticket grants no authority.

## Job, task, cancellation and revocation

```sh
vorra --state-dir STATE worker coordinator jobs --limit 16
vorra --state-dir STATE worker coordinator jobs --after-delegation-id LAST_RETURNED_UUID --limit 16
vorra --state-dir STATE worker coordinator inspect --delegation DELEGATION_UUID
vorra --state-dir STATE worker coordinator inspect --task LOCAL_TASK_UUID
vorra --state-dir STATE worker coordinator cancel --task LOCAL_TASK_UUID --delegation-id DELEGATION_UUID --attempt-id ATTEMPT_UUID --manifest-digest MANIFEST_SHA256 --request-id CANCEL_REQUEST_UUID
vorra --state-dir STATE worker coordinator revoke --peer EXECUTOR_DEVICE_UUID --request-id REVOKE_REQUEST_UUID
```

Jobs use exclusive ascending delegation UUID pagination, one through 32 records per page. The response's `next_after` is the actual next-request cursor; never synthesize a cursor by clock order. Inspect requires exactly one local task or delegation selector. Routine summaries omit copied files, manifest text and unaccepted raw result bodies. Actual compact inspections may contain an already accepted result report. The existing DTO validator checks response kind, IDs, cursor order, current phase and encoded byte limits before returning JSON.

Cancellation requires the exact immutable WorkerJobBinding as well as the local native task ID. Reuse those original delegation/attempt/digest values, not a newer attempt. Cancellation and purpose revocation record intent and retained state; neither proves remote effects stopped. Revocation is sticky and distinct from ordinary device revocation. There is no provider retry, cleanup attestation or artifact-apply action in this command group. Unknown remote results remain Unknown and retain their original charge/authority boundary.

## Owner release of an unresolved reservation

```sh
vorra --state-dir STATE worker coordinator release --task LOCAL_TASK_UUID --delegation-id DELEGATION_UUID --attempt-id ATTEMPT_UUID --manifest-digest MANIFEST_SHA256 --expected-phase outcome-unknown --operator NAME --reason TEXT --request-id RELEASE_REQUEST_UUID
```

Use `--expected-phase cancellation-requested` for a cancelled reservation that never received a result. `--request-id` is required. Rerun with the same ID and arguments to get the stored release back. A different ID after a release, a changed binding or a phase other than the expected one is rejected. Release frees the Host slot. It keeps the charge and any Unknown report, and makes later worker results for that task unacceptable. It does not prove the remote work stopped; confirm that out of band first.

## Request identity, errors and validation

Every mutation accepts `--request-id`; Delegate requires it. Other mutations generate a non-nil ID once only when it is omitted. Immediately before dispatch, stderr prints that stable identity and a small nonsecret operation summary. Prepare logs only kind and executor, never objective/context/input bytes. Cancel logs the original task/binding; Delegate logs the exact ticket/digest/fingerprint. Preserve the original selected file and argument values when considering an explicit replay. A request ID identifies the request but does not make every mutation replayable or grant renewed consent. The CLI performs no automatic retry, revision refresh or new identity substitution.

Cancellation before dispatch prevents the owner call. Cancellation/disconnect after dispatch can accompany a durable mutation; it does not establish rollback. Responses use the existing owner transport's bounded frames and request-aware checks, plus the coordinator DTO's matching validators. Ordinary coordinator replies are at most 256 KiB and full preparation replies at most 768 KiB within the existing two-MiB outer frame. Oversized/mismatched replies and actual host rejections remain explicit errors, not truncated success.

This leaf adds no dependencies, source comments, tests, secret arguments or environment lookup. The actual combined consumer `d273c52` passed all six normal-package Rust 1.88 gates. The actual Host credential-rejection/recovery walkthrough also passed; see [the validation checkpoint](worker.md#combined-host-validation-checkpoint) for precise limits. Live ticket consumption, file disclosure consent, remote admission/Offer/result ordering, cancellation/revocation replay, native cleanup, successful inference and platform behavior require their specific evidence. This command module alone claims none of those runtime criteria or production readiness.

The earlier isolated CLI leaf passed Rust 1.88 formatting and workspace/all-target checks with default features and all features. That historical compilation used the actual coordinator IPC patch, temporary host-module registration and explicit compilation-only unavailable handlers; the temporary wiring and package identity were excluded from the owned commit. Six then-unconsumed bridge/admission hooks remained warnings. An initial attempt failed before compilation because the build volume was full; both checks completed after capacity was restored. Those leaf checks supplied no strict Clippy/release or live owner/relay evidence; the actual combined static gate above uses the genuine consumers.

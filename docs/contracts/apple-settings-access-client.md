# Apple Settings query client

The iOS Settings screen talks to the Host's scoped remote Settings pipeline through one client: the original Access query path in `ReplicaService`, extended to every Host query kind (`access_status`, `snapshot`, `changes`, `update`, `host_action`, `session_open`, `session_send`, `session_poll`, `session_close`). The request and reply data are the [Apple wire](apple-remote-settings-wire.md); the Host side is the [transport](settings-access-transport.md). It sends no OwnerQuery/OwnerOperation, credential, invitation or secret, and never starts Bind.

## Gesture and original

- `ReplicaSettingsAccessGesture(service:scope:query:)` is a one-use MainActor capture: `ContinuousClock` before wall time, one query UUID, the exact Service, the connection scope token and the typed query. These are timing/correlation data, not authority.
- `requestSettingsAccess` requires the same Service, the current scope and an empty pending cell. A retained identical input returns its original handle; a spent input cannot mint another original.
- The generation for generation-bearing kinds comes only from the Service's own Access summary cache for the same connection scope and an unexpired grant. Callers cannot supply one. A query without such a summary, or whose keys are outside the summary's readable/writable sets, or a session query without `configured_provider` consent, is refused locally with `unavailable` before anything is sealed. The Host still rechecks everything.
- Request lifetime is at most 30,000 ms from the capture. Query UUID, encrypted request message ID and reply envelope ID are distinct identities. `request_id`, `open_request_id` and `turn_id` are chosen by the caller and reused verbatim for replay.

## Send window and receive

Unchanged from the original Access path: one outbound map counts ordinary sends and the single private original together (16), one send with no resend, private `Accepted` settles only the RAM cell (relay custody, never Saved), metadata-only replay state commits before ACK, unattributable deliveries are refused and ACKed per delivery, Store/revocation/integrity failures stay terminal and queue-full is not ACKed. Unsupported forms still go through the [pre-cache refusal](apple-private-settings-ingress.md).

## Adoption

`projectAccessDisplay` first requires `RemoteSettingsAccessRequest.accepts(reply)`: same Host and query UUID; for non-denials the reply generation equals the request generation and the body kind matches the query (Snapshot entries only for requested keys of this Host, a Changes page valid for the requested cursor/limit, the same Update `request_id`, the same host action, `session_accepted` with the same chat/turn, `session_status` for the same chat and text offset). Denials are accepted for any kind.

- An `access_status` summary or `grant_unavailable` answer to `access_status` replaces the Access display cache as before. `ReplicaSettingsAccessDisplayView.requestsAvailable` reports whether that summary can still supply a generation (current scope, Active summary, grant not expired); it is independent of the 30-second display freshness.
- Every other reply becomes one RAM `ReplicaSettingsQueryResult` keyed by the request handle. A `grant_unavailable` or `grant_changed` denial also clears the Access cache, so generation-bearing requests are refused until a new Access check.
- `settingsQueryOutcome(handle)` reports `pending` while the original or its send cell exists, then the received result, `accessObserved`, or the terminal local status (`cancelled`, `expired`, `interrupted`). Disable and an authenticated revocation clear the result cache.

## iOS composition

`AppModel` owns one serial driver (`HostSettingsStep`) that issues these queries one at a time while the Settings surface (root list or Ask Settings pane) is visible, the scene and app are active, no cover is shown and the relay is connected. Losing eligibility cancels the in-flight original and clears the queue; the Host keeps running. See [iOS client](../development/ios-client.md#connected-host-settings).

- Refresh: `access_status`, then (if an Update outcome is open) the exact same Update, then `snapshot` of the readable keys and `changes` pages from the last cursor (32 per page, at most 8 pages, last 32 hints kept). It runs automatically once per connection scope when the Settings root appears without current values.
- Save: one `update` with a new `request_id`, the revision of the values read on this connection and only granted writable keys. The mutation keeps its identity until a typed Update reply or a definite refusal (`conflict`, `outside_scope`, `write_blocked`, `invalid`, or `grant_unavailable`/`capacity` on the first attempt). Cancellation, expiry, interruption, `grant_changed`, `outcome_unknown` and `unavailable` leave it uncertain; the next refresh resends the same identity and the Host's journal replays the original receipt. Only a typed Update reply is shown as saved. `grant_changed` triggers one Access recheck and one retry.
- Host actions: `host_action` returns the Host's `host_action_required`; nothing executes.
- Ask Settings: `session_open` (Open identity kept for replay), `session_send` echoing the disclosed binding digest (turn identity kept until `session_accepted`), `session_poll` every 1.5 s while the turn runs and immediately while committed text remains, `session_close`. Status text is paged by byte offset at the same `text_revision`. New `operation_ids` trigger a values/changes refresh.

## Display

Values and change hints are bounded RAM state in `AppModel`. Values read on an earlier connection stay visible as stale read-only data; mutations require values from the current scope and a usable generation. Chat text stays in RAM and is shown as plain verbatim text; no control is derived from it.

## Legacy clients

The Bind, Snapshot, Original-inspection and Update clients ([Bind](apple-settings-bind-client.md), [Snapshot](apple-settings-snapshot-client.md), [inspection](apple-settings-original-inspection-client.md), [Update](apple-settings-update-client.md)) remain in source with no caller. Their packet shapes predate the Host and are rejected by it; the query client above replaces their role.

## Validation

Local compile/static checks and a disposable Simulator walkthrough of the shared Swift client against a real Host and relay ([record](../development/settings-validation.md#ios-remote-settings-client-walkthrough)). The SwiftUI screens and `AppModel` driver were compiled but not executed. Automated coverage is deferred to [#32](https://github.com/izyuumi/everything/issues/32).

# iOS committed-event notification attempts

The iOS client uses its existing ReplicaStore, strict device preference actor and NotificationService. It creates generic local notification attempts from committed authenticated conversation events while synchronization is enabled in the active app. An already committed event can be claimed during a transient relay outage; a current live socket is not inferred from the enabled choice. There is no background delivery, APNS service, model request or second notification delegate.

## Authoritative inputs and initial catch-up

ReplicaStore applies a verified sync batch in one transaction containing new event insertion, projection, conversation cursor, encrypted replies and inbox completion. Notification eligibility is read only from those committed event rows. Task snapshots deliberately omit terminal tasks; cursor movement, task disappearance, remote reports and command replies never imply completion. A scheduled run's isolated execution conversation is normally not a known replica conversation, so its delivered `scheduled_run_finished` event is the only committed terminal fact the client receives.

Each actual conversation has at most one `notifications_v1_` metadata floor. An absent floor suppresses claims until the first successfully committed batch with `hasMore == false`; that same apply transaction initializes the floor to its actual durable through sequence. This also baselines existing stores that predate notification delivery. First historical catch-up is silent, including events already present before this increment. No historical notification queue or schema is introduced.

## Bounded atomic claims

ReplicaService selects one of at most64 actual conversations in round-robin order. One global claim/delivery worker and one pending-refresh Boolean own progress; a slow callback cannot build a notification task queue. Each claim checks the actual full pinned Host, local identity, active scope and canonical floor in the Store transaction. It reads a forward prefix of at most256 events and2097152 actual encoded bytes and validates each stored row against its decoded authenticated event and sequence.

The closed categories are:

- Completion: an actual `task_status_changed` event whose status is `completed`, or an actual `scheduled_run_finished` event whose validated notification outcome is `succeeded`.
- Attention: an actual `permission_requested` event whose exact current projected approval remains undecided and has no immutable decision. If its verified inspection exists, its request lifetime must still contain the claim time. Legacy requests without inspection can produce attention only while the exact request remains undecided.

Failed/cancelled tasks, failed/cancelled/skipped/unknown scheduled runs, text events, cursor changes, task snapshots and ordinary settings receipts produce no notice. One claim yields at most one summary per category. Summaries contain only a category and a stable opaque SHA256 identifier derived from a versioned domain and the actual local/Host/conversation/claimed-through/category identities. Notification bodies, titles and userInfo contain no conversation title, event/task/tool identifiers, tool arguments or model/user text.

The transaction advances the exact floor before returning notice summaries. SQL errors roll back the claim. After a successful claim, crash, cancellation, disabled preferences, unavailable preference storage, denied OS settings or submission errors may lose the notice permanently. Duplicate delivery, reconnect and relaunch do not re-alert the same claimed events. This is at-most-once attempt admission, not exactly-once OS delivery; failed claims can be attempted again only because their transaction did not commit. No fresh notification ID is generated to retry a consumed event.

## Saved preferences, OS observation and lifetime

After claim, the delivery path observes current OS settings without requesting permission. Every possible submission re-reads saved completion/attention values from the SAME DevicePreferenceStore actor bound to the actual Replica/Keychain identity. Drafts, save receipts and cached UI snapshots do not choose notification categories. The preference read has one coalesced owned task and is joined by the existing preference drains. Delivery does not require the Settings destination to be visible.

The original service, full Host pin, local preference owner/device identity and foreground generation remain fixed. App scene activity, actual UIKit activity and the platform resign latch are checked across awaits. A synchronous final MainActor check immediately precedes the actual UNadd call. Scope revalidation is point-in-time; it is not an atomic OS submission transaction or an instantaneous revocation guarantee. Application inactivity, disconnect, disable or identity retirement closes intake and joins the real claim/read/submission work through existing drains. Retiring scene, disconnect and disable callers capture their exact original delivery task before their first await, so a successor generation is not joined by an older cleanup. None stops the independent Host.

Saved preferences and OS permission are also point-in-time reads. A later native change can race an already prepared OS request; no lock is held across OS callbacks and no instantaneous removal of already presented alerts is claimed.

NotificationService retains the existing PrivacyNotificationDelegate and permits only one submission. An explicit UNadd completion-handler continuation stays owned until its actual callback; cancelling a wrapper does not certify OS completion. If that callback finishes after retirement, pending removal is requested with the same opaque identifier. Removal has no completion proof and an alert may already have appeared. There is no hard OS callback/shutdown deadline or OS-stop guarantee.

Fixed delivery status is shown separately from authoritative preference-save and OS-authorization results. An accepted UNadd request does not prove presentation, user attention or an Applied settings outcome. Saved preferences remain usable without the Host; actual notification attempts require synchronization enabled, the same active local scope and the foreground client. OS permission still requires the existing separate native gesture.

## Validation boundary

Source parsing and review do not demonstrate notification authorization, actual presentation, foreground races, preference persistence or disposable device behavior. Supported SDK/static CI and separately recorded manual observations remain required. Automated coverage stays deferred under issue #32; issues #28, #54 and #117 retain their broader integration and runtime acceptance requirements.

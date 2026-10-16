# Restricted Settings agent source

The #113 runtime implementation merged in PR #143, adding an on-demand Settings conversation to the existing Rust Host. Actual exact-tree PR CI passed all seven Linux/macOS compilation/static/release forms and Rust1.88 MSRV. Later integration heads need their own applicable checks; disposable runtime verification remains pending. This document describes the implemented source boundaries; it does not establish working inference, an Apple screen, native platform verification or security readiness. Deferred automation remains [#32](https://github.com/izyuumi/everything/issues/32).

## Ownership and execution

`SettingsAgentRuntime` is a bounded command handle. Its Host-owned `SettingsAgentSupervisor` drives the existing `Agent::run_tools` native loop; it does not implement another inference loop. The Host retains the supervisor in its existing worker JoinSet. A disconnected private client drops its reply receiver, while accepted work remains supervised.

A Settings Agent clones the actual leased Agent, keeps its execution/admission/write maps and StateLease, and selects a fileless SQLite Store for this profile's conversations, checkpoints, events, task state, tool arguments and results. The profile retains the original ordinary Store only for bounded typed namespace reservations and turn fences. Markdown, inbox, relay, consolidation, embeddings and ordinary conversation publication have no Settings intake route.

The execution registry and Tool Broker are the actual Host instances. Seven Settings adapters are registered after the existing General selector catalog snapshot is constructed and before the final execution registry is shared. The catalog snapshot contains no Settings adapter back-reference. This ordering avoids a Registry → SettingsTools → ServiceCore → Composition → selector → Registry ownership cycle. The General selector remains metadata and authorization infrastructure, with no second executor.

The private profile does not retain a Broker or Agent. SettingsTools retains the existing SettingsService and weak references to actual child sessions/profiles; completion observers retain metadata and worker guards without an Agent/Broker reference. Actual native jobs and blocking closures retain the owning Agent/lease until their owned work ends.

## Intake and authority

The private native Settings pipe exposes AgentOpen, AgentSend, AgentPoll and AgentClose. The existing native session proof is authenticated before its parent identity reaches the manager. The manager creates and retains the actual opaque child through the same SettingsService Authority; a serialized chat UUID cannot create that child or its capability.

Open captures one configured native provider and returns its exact model, recipient, nonsecret binding digest, tool names and original expiry. It sends no model request. Send must echo the disclosed binding digest as its explicit acceptance. No missing-provider, credential, recipient or model fallback is selected. A missing provider leaves native controls usable.

The profile pins the captured actual provider handle. At the native pre-stream boundary, it takes the same Host mutation guard used by canonical configuration and credential mutations and rechecks the binding/source generation. The guard moves into the actual blocking worker that performs composite admission, original coarse attempt accounting and the private InFlight save. It is released before display yields and HTTP. Later canonical changes cannot retarget that admitted immutable handle. Low-level external library writes and external OS credential changes are outside this guard; the implementation does not claim universal external-vault monitoring or instantaneous network cancellation.

Exactly the seven names in [the Settings contract](../contracts/settings.md#exactly-seven-logical-tools) are offered and executable. Opaque profile contexts are checked in both directions at Broker and registry boundaries. Ordinary run options, Host configuration, scheduler policies, scheduled targets, worker manifests, public authority/child constructors and ordinary permission inspection reject the internal Settings mode. The private route does not invoke the ordinary Authorizer with Settings arguments or create ordinary approval rows.

## Service effects and closure

The adapters invoke the same SettingsService Core and existing typed operation journal. Call scope, principal, child identity and mutation ID come from authenticated server state and the actual Broker operation. Reads are redacted; updates are revision checked; protected actions require their existing native confirmation. Tool text cannot confirm a patch, reveal credentials, generate pairing material or approve device trust.

Before mutation service admission, the original Broker-derived ID is linked to the coarse fence. Missing receipt rows retain uncertainty. Existing receipts must match that exact Settings child, native parent and principal. Later service operation insertion cannot adopt a prelinked ID for another origin.

Invocation witnesses close their own intake and await real service work, including detached update children and actual blocking closures. A zero worker count does not classify effects. The source separately requires a trusted read-only/pre-admission disposition or an exact durable service receipt. Awaiting native approval is an ownership transfer to the existing service; InFlight, Applying and Unknown are not treated as completed effects.

Closing a chat retires its profile, cancels only its actual child and waits the owned native, provider, Broker and service work. The actual private foreground task is reconciled before a terminal fence; failed bookkeeping preserves the original fence. It preserves the native parent, siblings and Host lifetime. Settled private journals are released only after actual closed/idle checks and the original durable terminal fence. Unresolved journals are retained. Provider-only joined interruption/unusable-response cases have separate terminal states under [the fence contract](../contracts/restricted-settings.md).

## Persistence and recovery

Migration 0014 reserves immutable Settings identities and excludes ordinary writes at SQL insert/update roots in both directions. Markers contain nonsecret IDs, digests, original times, conservative counters and mutation links. They contain no Settings chat, provider continuation, raw tool arguments or read results. Existing typed Settings operation receipts remain the service's operational records.

RAM loss permanently retires that chat. A durable marker never authorizes reconstruction or a paid retry. Startup discovers unsettled fences and quarantines the same Broker before General intake. Native controls and authenticated inspection remain separate from that Broker quarantine.

The owner-only uncertainty acknowledgement requires the actual leased, draining Host, joined General workers, private supervisor quiescence, the real Settings close-worker witness and a drained same-Broker admission/exclusion gate. It inspects the exact original fence and linked service receipts and records an immutable digest-bound acknowledgement without changing the original phase or counters. Missing/Unknown service receipts remain explicitly accepted uncertainty. No ordinary ToolCall is fabricated.

During shutdown, unresolved Settings fences keep the Host's cleanup loop open. The private owner socket continues accepting inspection/cleanup calls until the actual recovery predicates pass. The local server stop token is cancelled only after that loop. See [the private contracts](../contracts/restricted-settings.md) and [validation limits](../development/restricted-settings.md).

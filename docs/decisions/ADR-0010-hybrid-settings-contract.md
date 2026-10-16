# ADR-0010: Native settings with a restricted assistant

## Status

Accepted for #108/#118. The typed contract, shared service, native actions, restricted profile and macOS controls/assistant are integrated through #163 with supported compilation/static checks. Scoped remote Settings remains #116 work. Native UI, provider, OS and security acceptance retain their separate gates.

## Context

The existing revisioned SettingsRepository and native clients already manage host state independently of models. Adding assistant chat must not create a second configuration authority, grant ordinary paired devices management permission, or turn model text into secret-entry, OS or device-trust authority.

## Decision

Keep native controls first-class and route their future typed settings commands through the same validated service as the assistant, with different authenticated actor ceilings. Distinguish host configuration, this-device preferences and read-only OS observations. Limit the immutable Settings runtime to exactly seven logical settings/device tools, without the general registry, wiki/main-history context or external selection. Capture each run's already authorized provider, keep chat session-scoped, and preserve native usability when no model exists.

Protected diff confirmation and closed native actions remain authenticated, exact, expiring service operations. Credentials and invitation secrets stay on ephemeral native-owner paths. Remote settings access requires an explicit per-key/session grant; owner security, recipient consent, credential and trust operations remain local-owner-only. Preserve desired, saved and effective state and reconcile uncertain outcomes using original identities.

## Consequences

The small [shared contract](../contracts/settings.md) gates parallel service, native-action and device work. Mac controls/chat can ship before optional remote/iOS integration. Existing Host persistence, E2EE, credential and native runtime mechanisms remain authoritative; no settings database, microservice, generated UI framework or second agent loop is added. Dynamic disable/rebuild behavior, exact confirmations, secret-free transport, restricted execution and Apple/device walkthroughs must be implemented and verified in their owning issues. The final #118 reconciliation reuses this ADR rather than creating another decision for the same boundary. Security review and automated coverage remain separate.

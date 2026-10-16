# Workflows

Read the issue and its dependencies before editing. Use a dedicated branch and worktree per independent issue. Coordinate shared module and manifest ownership with the integration owner. Do not guess changing interfaces; merge and review prerequisites first.

Run the [backlog synchronization workflow](backlog-sync.md) at startup, every five minutes while active, immediately after each PR merge, issue completion or milestone, and before reporting completion. Earlier snapshots never override edited issues. The coordinator must reconcile changed requirements, dependencies and assignments before accepting an implementation.

Each cycle also audits current PR heads, CI diagnostics and obsolete replacements. After merges, notify the dedicated cleanup agent and follow [repository maintenance](repository-maintenance.md) for source-preserving remote-ref cleanup and durable restart receipts; the coordinator retains merge and issue authority.

Implement the scoped behavior, update relevant documentation and status, validate, and commit logically related changes. Open a focused PR referencing the issue and deferred testing tracker. Review the diff and required GitHub checks before merging. Do not bypass branch protection. Close implementation issues only after their scoped implementation is integrated and limitations are accurately recorded.

Use the [POC CI workflow](poc-ci.md) for normal integration: Ubuntu formatting plus strict default/all-feature Clippy and the Apple compatibility gate. The Apple workflow performs real supported-SDK build/analyze only for its relevant Apple/shared-protocol paths; detection or required SDK failures block its gate. Normal workflow pushes target main, PRs validate every branch, and superseded runs cancel within the same workflow/event/PR-or-ref group. A branch push does not duplicate its PR run. Verify that repository rules actually require the stable contexts before claiming enforced protection.

Dispatch Extended validation explicitly when macOS Rust, MSRV, optimized builds or a default Linux CLI artifact are needed. The `retain_runtime_artifact` input belongs to that workflow, defaults to false and remains independent of ordinary PR checks. Dispatch Local embedding benchmark separately for public-corpus performance work. Do not require these expensive manual workflows for every POC merge or treat artifact acquisition as runtime verification. Record exact-head results and unverified behavior, and avoid further commits after passing CI unless a real finding requires a change.

When blocked, record the exact missing credential, platform, upstream capability, or integration dependency, then continue independent work.

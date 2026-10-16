# Repository maintenance

Repository maintenance accompanies the [backlog synchronization cycle](backlog-sync.md). The coordinator assigns a dedicated branch-cleanup agent, audits PR lifecycle and CI, and retains responsibility for source integration, issue state and publication. The cleanup agent inventories refs and performs only authorized normal remote-branch deletion. It does not merge PRs, change issues, handle credentials or remove local source and evidence.

## Each orchestration cycle

Run at startup, at least every five minutes during active orchestration, immediately after a merge, and before reporting completion. These are coordinator/agent operations, not a new application service, cron job or CI workflow.

1. Refresh authoritative issues and comments under the existing synchronization rules. Read the durable coordination and cleanup ledgers, including ownership and unresolved failures.
2. Read actual remote main, every open PR's head and base identities, and the current workflow/check results for each exact head. Include dependency PRs and active local branches/worktrees; an old passing run does not validate a changed head or an unreviewed source union.
3. Route real CI diagnostics to the source owner for a focused correction. Rerun a transient infrastructure/startup failure against the current head when appropriate, recording the original failure and new run identity. Do not replace a real diagnostic with repeated reruns, infer success from historical results, or bypass required checks, review or protection settings. Report current unavailable validation separately from historical billing failures.
4. Review obsolete or superseded PRs with the coordinator. Before normal closure, record the replacement PR or integration, exact preserved source ref and remaining issue scope in the closure explanation. Closing a PR does not complete or close its implementation issue. Preserve unpublished work and do not describe a replacement as merged or verified before it is.
5. After a normal merge, synchronize the backlog immediately, reconcile dependent PR bases and source ownership, and notify the cleanup agent with the merged PR/head and actual main SHA. Any subsequent union or retarget needs its own diff review and applicable exact-head checks. Complete the safe ref scan below and record retained/deferred reasons.

The coordinator serializes conflicting source and ref mutations. Notify active owners before changing their branch or dependency disposition. A cleanup finding does not authorize an agent to edit another owner's worktree or fix a PR under review.

## Remote branch eligibility

Every deletion requires fresh checks of the repository/ref identity, exact current SHA, actual remote main ancestry, branch protection, open PR heads **and bases**, live assignments, dependency references and local preservation. Keep default/main/protected branches and every active or explicitly retained branch, including coordination and pending implementation branches. A closed or merged PR label alone is insufficient.

| Candidate | Required disposition |
| --- | --- |
| SHA is an ancestor of actual main | Delete the remote ref only if no protection, PR dependency or live assignment requires it; retained historical worktree references follow the exact-release rule below. |
| Only an inactive historical worktree still references the branch | Obtain the coordinator's release for that exact branch/SHA before remote deletion; retain the local branch, worktree and evidence. |
| Obsolete SHA is not an ancestor of main | Require an exact retained local ref, reviewed replacement/source-preservation proof and an explicit coordinator release for that branch/SHA. A replacement link or merge label by itself is insufficient. |
| SHA, ownership, protection or PR dependencies changed, or any check failed | Defer, retain the branch and record the reason; refresh before reconsidering it. |

Re-read eligibility immediately before an authorized normal deletion and serialize it with coordinator ref activity. Use normal remote deletion only: no force push, reset, source discard or protection override. Record the command/API result and independently verify remote absence. A failed command or unconfirmed absence is not success. A recreated ref is a new candidate; an earlier deletion receipt does not release its new contents. This workflow retains every local branch, worktree, commit and evidence file; local cleanup is outside its scope.

Routine actions within the user's delegated maintenance scope do not need repeated user confirmation. The coordinator's exact-SHA releases reconcile current ownership and preservation evidence; they are not a substitute for missing implementation or required CI.

## Durable ledger and restart

Resolve the shared Git directory with `git rev-parse --path-format=absolute --git-common-dir`. The cleanup ledger lives at `<git-common-dir>/everything-orchestration/branch-cleanup/checkpoint.json`, separate from the authoritative backlog checkpoint. The coordinator records its location and the external working scan/report paths in the orchestration checkpoint. Workflow/run/check identities and live assignments remain in the coordinator's PR/orchestration ledger; cleanup records reference that ownership and review rather than creating a second CI authority. Do not assume a cleanup executable or network watcher is shipped by this repository.

Persist each completed scan and mutation receipt atomically, including file and directory synchronization. Retain:

- The last successful scan, next due time, actual main SHA and repository identity.
- Open PR head/base SHAs, coordinator ownership/review references, branch/worktree refs and protected/retained dispositions.
- Exact coordinator releases, retained local refs and replacement/source-preservation proof, with reasons for every deferred candidate.
- Normal deletion attempts, observed old SHA, result and independently verified absence, plus failures or incomplete scans.

A failed or partial scan must not advance the last successful scan time. An owned read-only watcher may assist active orchestration, but deletion remains a separate authorized invocation with current eligibility checks. Its failure is recorded, never reported as a completed scan. No watcher is an application-lifetime dependency or a replacement for coordinator review.

After restart or compaction, recover the ledgers, preservation refs, releases and assignments, then fetch fresh remote state before acting. Deliberately restart any owned scanner/watcher; it neither schedules agent turns nor recovers itself on a replacement machine. Reconcile any attempted deletion with the actual ref instead of replaying it blindly. Keep historical receipts and source identities even when remote refs disappear. Record remaining CI, platform and acceptance limits honestly; a tidy branch list is not repository completion.

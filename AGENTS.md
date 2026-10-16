# Everything development

Everything is a local-first native Rust AI host with an optional blind relay and native Apple clients. The host owns execution and durable state. Start at [docs/INDEX.md](docs/INDEX.md) and [implementation status](docs/status/implementation.md).

For scheduling, read the [architecture and module map](docs/architecture/scheduling.md), [owner CLI](docs/development/scheduling-cli.md), and [execution policy](docs/contracts/scheduled-execution.md). ScheduleEngine admits occurrences into the existing native Scheduler; preserve that single-runtime boundary and the captured authority, deadline and replay identities.

For hybrid Settings, start at [Settings architecture](docs/architecture/settings.md), [the restricted assistant](docs/architecture/settings-agent.md) and [setup/recovery](docs/development/macos-client.md). The [typed contract](docs/contracts/settings.md) defines authority ceilings; SDK compilation is separate from native UI, provider and remote acceptance.

- GitHub issues are the continuously updated authoritative backlog. Run `python3 scripts/sync_issues.py` on startup, at least every five minutes during active work, after every PR merge, completed issue or milestone, and before any completion claim. Read changed requirements/comments and #30, reconcile dependencies, notify affected owners immediately, and dispatch newly unblocked work. Follow [backlog synchronization](docs/development/backlog-sync.md).
- Audit PR heads, CI and obsolete replacements each active cycle; notify the dedicated cleanup agent after merges. Follow [repository maintenance](docs/development/repository-maintenance.md) for durable receipts, exact-SHA releases and preservation of local source.
- Keep one native runtime and simple, cohesive modules. No other harness compatibility, backup subsystem, mandatory cloud service, or custom cryptography.
- Never add source-code comments or new automated tests at this stage. Preserve existing tests. Track deferred testing in the consolidated follow-up issue linked in status.
- Never commit credentials or user data. Validate inputs, bound resources, propagate cancellation, and report errors. Do not represent incomplete features as working.
- Use isolated worktrees and focused issue branches. One owner per file; merge prerequisites and inspect stable contracts before dependent work. The coordinator owns Cargo.toml, src/lib.rs, integration, and shared status.
- Update architecture, contracts, status, and consequential ADRs when behavior changes. Docs describe implemented reality.
- Normal Rust validation is `cargo fmt --all --check` and strict `cargo clippy --workspace --all-targets --locked -- -D warnings`, also with `--all-features`. Relevant Apple/shared-protocol changes require supported Xcode build/analyze. Use [POC CI](docs/development/poc-ci.md) for the blocking contexts and separate manual macOS Rust, MSRV, optimized builds, CLI artifacts and embedding measurements. Run focused manual checks on disposable data; compile/static checks do not prove production or security readiness.
- Open and review focused PRs. Respect required checks; close issues only when implementation criteria are met. Report unavailable validation explicitly.

See [setup](docs/development/setup.md), [conventions](docs/development/conventions.md), [workflows](docs/development/workflows.md), and [contracts](docs/INDEX.md#contracts).

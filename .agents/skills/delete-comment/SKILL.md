---
name: delete-comment
description: Classify and remove source comments, preserve decisions in ADRs, and shrink the comment baseline.
---

Run `python3 scripts/check_comments.py --list` from the repository root. Classify each listed comment before editing.

| Kind | Action |
| --- | --- |
| Tautology | Delete. The code already shows what the comment says. |
| Stale | Delete. The comment refers to obsolete code or information, including commented-out configuration. |
| Excuse | Delete and file a GitHub issue for the underlying hack. Group related comments in one issue and include locations and a proposed fix. |
| Decision Record | Move the rationale to an ADR under `docs/decisions/ADR-NNNN-*.md` using the next free number, then delete the comment. |

Keep line-1 shebangs, Rust `// SAFETY:` comments on unsafe code, Python coding declarations, recognized lint and tool directives, and SPDX license headers. The linter marks these as allowed.

File issues only when authorized. Otherwise report proposed issue titles and bodies.

Run `python3 scripts/check_comments.py --update` to shrink the baseline, then run `python3 scripts/check_comments.py` to verify it. Run the normal Rust validation from `AGENTS.md` and report command results, counts per kind (including kept), and comment lines shed. Preserve existing tests and versions. Never add tests, fixtures, or test frameworks, and never bump versions.

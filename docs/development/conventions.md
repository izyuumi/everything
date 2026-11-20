# Conventions

Use idiomatic Rust and Swift with small cohesive modules, typed domain values, structured errors, bounded external inputs, and explicit cancellation. Avoid blocking async workers, global mutable state, unnecessary cloning, and panics on fallible production paths. Use established cryptographic libraries only.

Source code must contain no comments, including documentation comments. Explain architecture and contracts in Markdown. Do not create automated tests, fixtures, or test frameworks during this stage. Preserve any existing tests.

The comment ratchet rejects new comments and stale baseline entries. Run `python3 scripts/check_comments.py --list` to inventory comments, use the `/delete-comment` skill to clean them up, and run `python3 scripts/check_comments.py --update` to shrink the baseline.

All generated files and dependency lockfiles must be intentional. Never commit secrets, local user state, downloaded model weights, or machine-specific configuration.

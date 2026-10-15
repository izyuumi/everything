# Everything

A local-first native AI harness under construction. The Rust CLI supports persistent conversations and explicit native tool rounds with Anthropic and OpenAI APIs. The optional encrypted relay runs separately; graphical clients are not required. Background service orchestration and native clients are still being integrated.

```sh
cargo build --workspace --release --locked
cargo run --bin everything -- --help
cargo run --bin everything-relay -- --help
```

Initialize an offline conversation with `everything init --provider anthropic --model MODEL_ID`, then use its UUID with `everything chat --conversation UUID --message "Hello"`. Add `--tools --workspace /absolute/workspace` to a new chat for bounded native tool execution; read-only authority is the default. Credentials come from the selected provider environment variable. See [CLI usage](docs/development/cli.md) for state paths, recovery and validation limits. Successful live inference has not been verified in this development environment.

The optional [worker](docs/architecture/cloud-worker.md) reuses the native kernel. `everything --state-dir DEDICATED_PATH worker serve --config PATH` runs the scoped executor; `worker coordinator ...` uses an explicitly enabled running Host to review selected data and delegate a fresh root task. Worker-purpose identities, pins and credentials are separate. The dedicated executor has qualified failure/recovery observations; combined Rust gates and the production Host coordinator credential-rejection/recovery walkthrough passed. No cloud provisioning, arbitrary-code isolation or successful remote inference is claimed.

Read [documentation](docs/INDEX.md), [development rules](AGENTS.md), and [implementation status](docs/status/implementation.md). GitHub [roadmap #30](https://github.com/izyuumi/everything/issues/30) defines the implementation backlog. Automated testing is deferred by the current development policy; builds and manual validation do not establish production readiness.

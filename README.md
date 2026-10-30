# Everything

A local-first native AI harness under construction. The headless Rust Host and CLI share persistent conversations, native tool rounds, schedules, a Markdown Wiki and explicit permission/recovery controls. Optional encrypted relay and native Apple clients do not own Host lifetime. Native Anthropic and OpenAI adapters are implemented; successful live inference remains unverified.

```sh
cargo build --release --locked
cargo run --bin everything -- --help
cargo run --bin everything -- relay --help
```

Run `everything` with no subcommand for the full-screen [terminal UI](docs/development/tui.md); on first run it offers to start a Host and walks through connecting a model (ChatGPT sign-in or a tested API key). Initialize an offline conversation with `everything init --provider anthropic --model MODEL_ID`, then use its UUID with `everything chat --conversation UUID --message "Hello"`. Add `--tools --workspace /absolute/workspace` to a new chat for bounded native tool execution; read-only authority is the default. Credentials come from the selected provider environment variable. See [CLI usage](docs/development/cli.md) for state paths, recovery and validation limits. `everything relay` runs or administers the optional blind relay server and turns this Host's relay connection on or off ([relay](docs/architecture/relay.md)). Successful live inference has not been verified in this development environment.

The merged Host composition keeps embeddings and external System One classification opt-in. Production embeddings use only the strict Linux embedding helper; unavailable confinement or unsupported platforms retain real FTS fallback. The optional scoped Worker coordinator/executor is a current-main source candidate awaiting fresh exact-head CI and runtime validation; read [Worker CLI and limits](docs/development/worker-coordinator-cli.md) before explicit delegation. It provides no arbitrary-code sandbox or hard OS quota guarantee.

Read [documentation](docs/INDEX.md), [development rules](AGENTS.md), and [implementation status](docs/status/implementation.md). GitHub [roadmap #30](https://github.com/izyuumi/everything/issues/30) defines the implementation backlog. Automated testing is deferred by the current development policy; builds and manual validation do not establish production readiness.

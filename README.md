# Vorra

A local-first native AI harness under construction. The headless Rust Host and CLI share persistent conversations, native tool rounds, schedules, a Markdown Wiki and explicit permission/recovery controls. Optional encrypted relay and native Apple clients do not own Host lifetime. Native Anthropic and OpenAI adapters are implemented. Live inference was verified on 2026-10-21 through a real provider: chat, multi-turn, tools, approvals, memory, schedules and cancel.

```sh
cargo build --release --locked
cargo run --bin vorra -- --help
cargo run --bin vorra -- relay --help
```

Prebuilt macOS and Linux binaries install with `curl -fsSL https://raw.githubusercontent.com/izyuumi/everything/main/install.sh | sh` (add `-s -- --beta` for the beta) and update in place with `vorra update` (`--beta`, `--check`); see [CLI releases](docs/development/releases.md).

Vorra was formerly named Everything. Install the first Vorra release manually once with `install.sh`: an old `everything update` refuses the new signing identifier and cannot find the new `vorra-<target>.tar.gz` asset. The first `vorra` run moves `~/.everything` to `~/.vorra`. Then run `vorra chatgpt sign-in` again and re-enter any vault, MCP or System One credentials, because keychain items are stored under new `to.yumi.vorra.*` service names. See [first Vorra release](docs/development/releases.md#first-vorra-release-install-manually-once).

Run `vorra` with no subcommand for the full-screen [terminal UI](docs/development/tui.md); on first run it offers to start a Host and walks through connecting a model (ChatGPT sign-in or a tested API key). Initialize an offline conversation with `vorra init --provider anthropic --model MODEL_ID`, then use its UUID with `vorra chat --conversation UUID --message "Hello"`. Add `--tools --workspace /absolute/workspace` to a new chat for bounded native tool execution; read-only authority is the default. Credentials come from the selected provider environment variable. See [CLI usage](docs/development/cli.md) for state paths, recovery and validation limits. `vorra relay` runs or administers the optional blind relay server and turns this Host's relay connection on or off ([relay](docs/architecture/relay.md)).

The merged Host composition keeps embeddings and external System One classification opt-in. Production embeddings use only the strict Linux embedding helper; unavailable confinement or unsupported platforms retain real FTS fallback. The optional scoped Worker coordinator/executor is a current-main source candidate awaiting fresh exact-head CI and runtime validation; read [Worker CLI and limits](docs/development/worker-coordinator-cli.md) before explicit delegation. It provides no arbitrary-code sandbox or hard OS quota guarantee.

Read [documentation](docs/INDEX.md), [development rules](AGENTS.md), and [implementation status](docs/status/implementation.md). GitHub [roadmap #30](https://github.com/izyuumi/everything/issues/30) defines the implementation backlog. Automated testing is deferred by the current development policy; builds and manual validation do not establish production readiness.

# Everything

A local-first native AI harness under construction. The Rust host and optional relay are independent command-line binaries. No agent runtime is implemented yet.

```sh
cargo build --workspace --release --locked
cargo run --bin everything -- --help
cargo run --bin everything-relay -- --help
```

Read [documentation](docs/INDEX.md), [development rules](AGENTS.md), and [implementation status](docs/status/implementation.md). GitHub [roadmap #30](https://github.com/izyuumi/everything/issues/30) defines the implementation backlog. Automated testing is deferred by the current development policy; builds and manual validation do not establish production readiness.

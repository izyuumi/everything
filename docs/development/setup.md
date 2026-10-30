# Setup

Install Rust 1.88 or newer with rustfmt and clippy, plus a C compiler for bundled SQLite dependencies. The current native MCP SDK sets this compiler floor, as recorded in [ADR-0003](../decisions/ADR-0003-native-mcp-sdk.md). Clone the repository and run `cargo build --workspace --locked`.

Use `cargo run --bin everything -- --help`; the optional relay server and host relay toggle are `everything relay` subcommands (`cargo run --bin everything -- relay --help`). Apple development requires macOS and a supported Xcode installation; Linux cannot validate Apple runtime behavior.

Do not place credentials or runtime data in the repository. Provider and transport configuration will be documented alongside implemented commands.

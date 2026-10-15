# Setup

Install a current stable Rust toolchain with rustfmt and clippy, plus a C compiler for bundled SQLite dependencies as they are introduced. Clone the repository and run `cargo build --workspace --locked`.

Use `cargo run --bin everything -- --help` and `cargo run --bin everything-relay -- --help`. Apple development requires macOS and a supported Xcode installation; Linux cannot validate Apple runtime behavior.

Do not place credentials or runtime data in the repository. Provider and transport configuration will be documented alongside implemented commands.

# Help MCP server

`vorra-help-mcp` is a public, read-only MCP server that lets any AI agent help a user install, configure and use Vorra. It answers from the Vorra documentation and generated CLI and settings metadata embedded at build time. It cannot see or change anyone's installation, holds no credentials and does no runtime filesystem or network access beyond serving its own transport.

## Module boundaries

The server is the separate workspace member `crates/vorra-help-mcp` (`publish = false`) with its own binary. It depends only on `rmcp` (`=3.5.1`, the main crate's pin, with the server, macro, stdio and Streamable HTTP server features), `axum`, `clap`, `serde`, `serde_json`, `tokio` and `tokio-util`. It never links the `vorra` library, so the Host, vault, relay, storage and credential code are absent from the public binary. The release workflow still builds and ships only `vorra`.

- `build.rs` embeds `README.md`, `CHANGELOG.md` and every `docs/**/*.md` file, splits them into heading sections (ignoring `#` lines inside code fences) and resolves the install-guide sections. A renamed or removed install-guide heading fails the build.
- `manifest.json` holds the `vorra` command tree and the typed settings registry. The hidden `vorra help-manifest` subcommand writes it from the real clap definitions (`clap::CommandFactory`) and `SettingRegistry::from_profile(None)`. It runs before state-directory resolution, so it never touches `~/.vorra`. The Validate workflow regenerates it and fails on any difference, so commands and settings cannot drift silently. After changing a command or setting, run `cargo run --bin vorra -- help-manifest > crates/vorra-help-mcp/manifest.json`.
- `content.rs` serves embedded documents and a small section search: ASCII-lowercased term counts, heading matches weighted above body matches, and documents matching more query terms ranked first.
- `server.rs` is the MCP handler; `http.rs` is the HTTP transport, rate limit and the static pairing site.
- `site/` holds the pairing site files, embedded with `include_str!`: `apple-app-site-association`, `pair.html` and `pair.js`. They are the only copy in the repository.

## Contract

Every tool is annotated `readOnlyHint: true` and `openWorldHint: false`. Invalid input returns a JSON-RPC invalid-params error.

| Tool | Input | Result |
| --- | --- | --- |
| `search_docs` | `query` (at most 512 bytes), optional `limit` 1–20, default 8 | Ranked sections: `path`, `uri`, `heading`, `snippet` |
| `get_doc` | `path` or `vorra-docs://` URI | Full Markdown. Only embedded paths match exactly; anything else, including `../../etc/passwd`, is rejected |
| `get_install_guide` | `os`: `macos`, `linux` or `ios` | Markdown assembled from the install, onboarding, service and client sections, each with its source path |
| `list_config` | none | Every `SettingKey` descriptor: scope, constraints, default, sensitivity, agent mutability, apply behaviour |
| `explain_config` | `key` | The descriptor plus documentation sections that mention the key; unknown keys list the valid ones |
| `troubleshoot` | `symptom` | Matching sections, boosting failure, recovery, repair and limitation headings and development and status documents |
| `list_features` | none | Each architecture document's title, path and first paragraph, plus a pointer to implementation status |
| `list_commands` | none | Every `vorra` command path with its about text |
| `explain_command` | `name`, for example `schedule create` | Arguments (flags, value names, defaults, possible values, environment variables), subcommands and documentation that mentions `vorra <name>` |
| `latest_version` | none | The newest `CHANGELOG.md` release heading (also the server's MCP `Implementation` version, `unreleased` before the first release; `Cargo.toml`'s 0.1.0 is a placeholder and is never reported), the releases URL and how to check with `vorra update --check` |
| `changelog` | none | `CHANGELOG.md` as embedded, with a note when no stable release notes exist yet |

Resources: every embedded document as `vorra-docs://<path>` (`text/markdown`), listed by `resources/list` and returned by `resources/read`; an unknown URI returns resource-not-found. Prompt: `setup_vorra` with an optional `os` argument (`macos`, `linux` or `ios`) returns one user message that asks the agent to start from `get_install_guide` and use the other tools, and never to request secrets in chat.

There is no runtime GitHub release lookup. `latest_version` reports what was embedded at build time and tells the user how to query GitHub with their own `vorra` binary; a hosted server is only as current as its last build.

## Transports

`vorra-help-mcp` with no arguments serves stdio. `--http` serves Streamable HTTP in rmcp's stateless mode with JSON responses (no sessions, no SSE keep-alive), so any instance behind a reverse proxy can answer any request:

```sh
vorra-help-mcp --http --bind 127.0.0.1 --port 18790
curl -fsS http://127.0.0.1:18790/healthz
```

- MCP is served at `/mcp`; `GET /healthz` returns `200 ok` and is not rate limited. The three pairing-site routes below are also served.
- Request bodies over 64 KiB receive `413 Payload Too Large` from rmcp's streaming body limit.
- Each client address gets a token bucket of 60 requests refilled at one per second; an empty bucket receives `429 Too Many Requests` with `Retry-After: 1`. At most 4096 addresses are tracked; when the table is full of active clients, new addresses receive 429 rather than growing memory.
- rmcp validates the `Host` header against `localhost`, `127.0.0.1` and `::1`. Add a public name with `--allowed-host vorra.yumi.to` (repeatable).
- The rate limit keys on the TCP peer address. `--trust-forwarded-for` keys on the last `X-Forwarded-For` entry instead, which is the address the nearest proxy saw. Use it only when the server is reachable solely through one trusted proxy that appends that header, such as a Cloudflare tunnel to a loopback bind; otherwise any client can choose its own key.
- No authentication. Origin validation is off, matching rmcp's default; the server returns only public documentation.

## Pairing site

The same HTTP server serves the static files for iPhone pairing links on `vorra.yumi.to` (see the [pairing contract](../contracts/device-pairing.md#link)). They are read-only `GET` routes (`HEAD` works, other methods get `405`), share the rate limit above, and answer any `Host` header, including `vorra.yumi.to`:

| Path | Content-Type | Other headers | Body |
| --- | --- | --- | --- |
| `/.well-known/apple-app-site-association` | `application/json` | `Cache-Control: max-age=3600`, `X-Content-Type-Options: nosniff` | `applinks` for app ID `AN5KM8QGEF.to.yumi.vorra.ios`, components `/pair` and `/pair/*` |
| `/pair` | `text/html; charset=utf-8` | `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`, `Referrer-Policy: no-referrer`, `nosniff`, `Cache-Control: no-store` | Fallback page: how to get the app (App Store `https://apps.apple.com/app/id6829295744`, TestFlight while in beta) and run `vorra pair` again |
| `/pair/open.js` | `text/javascript; charset=utf-8` | Same policy headers, `max-age=3600` | `location.replace("vorra://pair" + location.hash)` when a fragment is present |

The fragment that holds the pairing invitation is never sent to the server, which only ever sees `/pair`. The page makes no requests besides its own script; the CSP blocks every other fetch, and the App Store link sends no referrer. Requests to `/pair/` or other paths under it return `404`. The universal link itself never loads the page when Vorra is installed and iOS has fetched the association file.

## Use from an agent

Build and install the binary from a clone (it is not published to crates.io or attached to releases):

```sh
cargo install --locked --path crates/vorra-help-mcp
```

The hosted endpoint `https://vorra.yumi.to/mcp` is **planned and not yet deployed**. Only the `/mcp` path on `vorra.yumi.to` is this server; the same host also serves other Vorra paths such as the pairing `/.well-known/apple-app-site-association` file and `/pair`, so the proxy routes `/mcp` (and optionally `/healthz`) here and nothing else. The planned deployment runs `vorra-help-mcp --http --allowed-host vorra.yumi.to --trust-forwarded-for` on loopback behind a Cloudflare tunnel. Build that binary with `RUSTFLAGS="--remap-path-prefix=$HOME=/home" cargo build --release --locked -p vorra-help-mcp`: an ordinary build keeps the builder's `~/.cargo/registry` paths in dependency panic locations, though no embedded content contains a home path.

| Client | Local stdio | Hosted (planned) |
| --- | --- | --- |
| Claude Code | `claude mcp add vorra-help -- vorra-help-mcp` | `claude mcp add --transport http vorra-help https://vorra.yumi.to/mcp` |
| Claude Desktop | `claude_desktop_config.json`: `{"mcpServers": {"vorra-help": {"command": "vorra-help-mcp"}}}` | Settings → Connectors → Add custom connector, URL `https://vorra.yumi.to/mcp` |
| Codex | `codex mcp add vorra-help -- vorra-help-mcp`, or `~/.codex/config.toml`: `[mcp_servers.vorra-help]` with `command = "vorra-help-mcp"` | `~/.codex/config.toml`: `[mcp_servers.vorra-help]` with `url = "https://vorra.yumi.to/mcp"` |
| Cursor | `~/.cursor/mcp.json`: `{"mcpServers": {"vorra-help": {"command": "vorra-help-mcp"}}}` | `~/.cursor/mcp.json`: `{"mcpServers": {"vorra-help": {"url": "https://vorra.yumi.to/mcp"}}}` |

Use the absolute path to `vorra-help-mcp` when the client does not inherit a shell `PATH` that contains `~/.cargo/bin`.

## Verification

Compilation, strict Clippy and a manual MCP Inspector CLI walkthrough of every tool, resources and the prompt over stdio and HTTP, plus observed 413, 429, foreign-`Host` 403 and forwarded-client keying, are recorded on the introducing pull request for #261. No automated tests exist for this crate, consistent with the deferred-testing policy. The hosted deployment has not been exercised.

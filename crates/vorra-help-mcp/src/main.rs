use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::ExitCode;

use clap::Parser;
use rmcp::ServiceExt;

mod content;
mod http;
mod server;

#[derive(Parser)]
#[command(version, about = "Public read-only help MCP server for Vorra")]
struct Arguments {
    #[arg(long, help = "Serve Streamable HTTP instead of stdio")]
    http: bool,
    #[arg(long, requires = "http", default_value_t = IpAddr::V4(Ipv4Addr::LOCALHOST))]
    bind: IpAddr,
    #[arg(long, requires = "http", default_value_t = 18790)]
    port: u16,
    #[arg(
        long = "allowed-host",
        requires = "http",
        value_name = "HOST",
        help = "Accept this Host header in addition to loopback names, for example vorra.yumi.to"
    )]
    allowed_hosts: Vec<String>,
    #[arg(
        long,
        requires = "http",
        help = "Rate limit by the last X-Forwarded-For address; only behind one trusted reverse proxy"
    )]
    trust_forwarded_for: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let arguments = Arguments::parse();
    let manifest = match serde_json::from_str(include_str!("../manifest.json")) {
        Ok(manifest) => manifest,
        Err(error) => {
            eprintln!("vorra-help-mcp: embedded manifest is invalid: {error}");
            return ExitCode::FAILURE;
        }
    };
    let server = server::HelpServer::new(manifest);
    let result = if arguments.http {
        http::serve(
            server,
            http::HttpOptions {
                address: SocketAddr::new(arguments.bind, arguments.port),
                allowed_hosts: arguments.allowed_hosts,
                trust_forwarded_for: arguments.trust_forwarded_for,
            },
        )
        .await
        .map_err(|error| error.to_string())
    } else {
        stdio(server).await
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("vorra-help-mcp: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn stdio(server: server::HelpServer) -> Result<(), String> {
    server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|error| error.to_string())?
        .waiting()
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

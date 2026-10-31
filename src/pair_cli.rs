use std::time::Duration;

use clap::Args;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vorra::host::client::HostClient;
use vorra::host::pair;
use vorra::protocol::UnixMillis;

use crate::CliError;

const STOPPED: &str = "pairing stopped; the code shown is no longer valid";

#[derive(Args)]
pub struct PairArguments {
    #[arg(
        long,
        value_name = "1-600",
        default_value_t = 300,
        value_parser = clap::value_parser!(u64).range(1..=600),
        help = "Seconds each QR code stays valid before it is replaced"
    )]
    lifetime_seconds: u64,
    #[arg(
        long,
        help = "Host name shown on the phone [default: this computer's name]"
    )]
    name: Option<String>,
}

pub async fn run(
    client: &HostClient,
    arguments: PairArguments,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    let relay = pair::relay(client, cancellation.clone()).await?;
    if !relay.connected {
        eprintln!(
            "warning: this host is not connected to its relay {} yet; the phone cannot reach it until it is",
            relay.origin
        );
    }
    let name = arguments.name.unwrap_or_else(pair::host_name);
    let lifetime_ms = arguments.lifetime_seconds * 1_000;
    let lead_ms = (lifetime_ms / 4).min(30_000);
    let (sender, mut stdin) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        while std::io::stdin()
            .read_line(&mut line)
            .is_ok_and(|read| read > 0)
        {
            if sender.send(std::mem::take(&mut line)).is_err() {
                return;
            }
        }
    });
    loop {
        let invitation = pair::begin(
            client,
            &relay.origin,
            &name,
            lifetime_ms,
            cancellation.clone(),
        )
        .await?;
        let result = wait(
            client,
            &invitation,
            lead_ms,
            &mut stdin,
            cancellation.clone(),
        )
        .await;
        if !matches!(result, Ok(Outcome::Approved)) {
            let _ = pair::cancel(client, invitation.pairing_id, CancellationToken::new()).await;
        }
        match result? {
            Outcome::Refresh => println!("\nThat code expired. Here is a new one.\n"),
            Outcome::Approved => return Ok(()),
            Outcome::Rejected => return Err(CliError::Pairing("pairing rejected")),
        }
    }
}

enum Outcome {
    Refresh,
    Approved,
    Rejected,
}

async fn wait(
    client: &HostClient,
    invitation: &pair::Invitation,
    lead_ms: u64,
    stdin: &mut mpsc::UnboundedReceiver<String>,
    cancellation: CancellationToken,
) -> Result<Outcome, CliError> {
    let mut stdout = tokio::io::stdout();
    let now = UnixMillis::now().map_err(|_| CliError::Pairing("system clock is invalid"))?;
    let seconds = invitation.expires_at_ms.saturating_sub(now.get()) / 1_000;
    let screen = format!(
        "Scan with your iPhone camera to pair it with this host:\n\n{}\n\nOr open this link on the phone (it is secret; do not share it):\n{}\n\nWaiting for the phone… (code valid for {seconds}s, Ctrl+C to stop)\n",
        invitation.qr.as_str(),
        invitation.link.as_str()
    );
    stdout.write_all(screen.as_bytes()).await?;
    stdout.flush().await?;
    loop {
        let now = UnixMillis::now()
            .map_err(|_| CliError::Pairing("system clock is invalid"))?
            .get();
        if now + lead_ms >= invitation.expires_at_ms {
            return Ok(Outcome::Refresh);
        }
        let requests =
            match pair::requests(client, invitation.pairing_id, cancellation.clone()).await {
                Err(_) if cancellation.is_cancelled() => return Err(CliError::Pairing(STOPPED)),
                result => result?,
            };
        if let Some(request) = requests.first() {
            while stdin.try_recv().is_ok() {}
            let prompt = format!(
                "\n{} wants to pair  code {}  device {}\nApprove only if your phone shows this same code; the name is chosen by the phone.\nApproving lets it read all conversations, send messages and answer permission prompts.\nApprove? [y/N] ",
                request.device_name,
                request.code,
                pair::short_fingerprint(&request.fingerprint)
            );
            stdout.write_all(prompt.as_bytes()).await?;
            stdout.flush().await?;
            let deadline = Duration::from_millis(request.expires_at_ms.saturating_sub(now));
            let read = tokio::select! {
                _ = cancellation.cancelled() => return Err(CliError::Pairing(STOPPED)),
                read = tokio::time::timeout(deadline, stdin.recv()) => read,
            };
            let approve = match read {
                Err(_) => {
                    println!();
                    return Ok(Outcome::Refresh);
                }
                Ok(answer) => answer.is_some_and(|answer| {
                    matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES")
                }),
            };
            if approve {
                println!("Sending the approval to the phone…");
            }
            let decision = pair::decide(client, request, approve, cancellation.clone()).await?;
            return Ok(if decision.approved {
                println!("paired: {}", decision.label);
                Outcome::Approved
            } else {
                println!("rejected: {}", decision.label);
                Outcome::Rejected
            });
        }
        tokio::select! {
            _ = cancellation.cancelled() => return Err(CliError::Pairing(STOPPED)),
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

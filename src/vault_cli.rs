use std::io::{self, IsTerminal, Read, Write};

use clap::{Args, Subcommand, ValueEnum};
use everything::host::chatgpt::ChatGptOperation;
use everything::host::client::HostClient;
use everything::host::credentials::SecretTransfer;
use everything::host::settings::{CredentialStorage, ModelCredentialSource};
use everything::ipc::{OwnerOperation, OwnerOperationResult, OwnerQuery, OwnerQueryResult};
use everything::protocol::CommandId;
use everything::vault::{
    MAX_VAULT_LIST, ProviderStatus, VaultError, VaultItemKind, VaultItemMetadata, VaultProviderId,
    VaultProviderState, VaultPutRequest, VaultReference,
};
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::queries::{label, print_lines};
use crate::{CliError, write_json};

const MAX_SECRET_BYTES: usize = 8_192;

#[derive(Args)]
pub struct VaultArguments {
    #[command(subcommand)]
    command: VaultCommand,
}

#[derive(Subcommand)]
enum VaultCommand {
    Status(JsonArguments),
    List(VaultListArguments),
    Add(VaultAddArguments),
    Remove(VaultRemoveArguments),
    Connect(VaultConnectArguments),
    Disconnect(VaultConnectArguments),
}

#[derive(Args)]
struct JsonArguments {
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct VaultListArguments {
    #[arg(long, default_value_t = 20)]
    limit: usize,
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum KindArgument {
    Login,
    ApiSecret,
    Card,
    Identity,
}

impl From<KindArgument> for VaultItemKind {
    fn from(value: KindArgument) -> Self {
        match value {
            KindArgument::Login => Self::Login,
            KindArgument::ApiSecret => Self::ApiSecret,
            KindArgument::Card => Self::Card,
            KindArgument::Identity => Self::Identity,
        }
    }
}

#[derive(Args)]
struct VaultAddArguments {
    #[arg(value_enum)]
    kind: KindArgument,
    #[arg(long)]
    label: String,
    #[arg(long = "site", value_name = "HOST")]
    sites: Vec<String>,
    #[arg(long)]
    username: Option<String>,
    #[arg(long, value_name = "NAME")]
    env_name: Option<String>,
    #[arg(long)]
    service: Option<String>,
    #[arg(long)]
    brand: Option<String>,
    #[arg(long, value_name = "DIGITS")]
    last4: Option<String>,
    #[arg(long, value_name = "1-12")]
    expiry_month: Option<u8>,
    #[arg(long, value_name = "YEAR")]
    expiry_year: Option<u16>,
    #[arg(long = "field", value_name = "FIELD")]
    fields: Vec<String>,
    #[arg(long)]
    json: bool,
}

impl VaultAddArguments {
    fn metadata(&self) -> Result<VaultItemMetadata, CliError> {
        let kind = VaultItemKind::from(self.kind);
        let flags = [
            ("--site", !self.sites.is_empty(), VaultItemKind::Login),
            ("--username", self.username.is_some(), VaultItemKind::Login),
            (
                "--env-name",
                self.env_name.is_some(),
                VaultItemKind::ApiSecret,
            ),
            (
                "--service",
                self.service.is_some(),
                VaultItemKind::ApiSecret,
            ),
            ("--brand", self.brand.is_some(), VaultItemKind::Card),
            ("--last4", self.last4.is_some(), VaultItemKind::Card),
            (
                "--expiry-month",
                self.expiry_month.is_some(),
                VaultItemKind::Card,
            ),
            (
                "--expiry-year",
                self.expiry_year.is_some(),
                VaultItemKind::Card,
            ),
        ];
        if let Some((flag, ..)) = flags.iter().find(|(_, set, owner)| *set && *owner != kind) {
            return Err(CliError::VaultFlag(flag, label(kind)));
        }
        let label = self.label.clone();
        let metadata = match kind {
            VaultItemKind::Login => VaultItemMetadata::Login {
                label,
                sites: self.sites.clone(),
                username: self.username.clone(),
            },
            VaultItemKind::ApiSecret => VaultItemMetadata::ApiSecret {
                label,
                env_name: self.env_name.clone(),
                service: self.service.clone(),
            },
            VaultItemKind::Card => VaultItemMetadata::Card {
                label,
                brand: self.brand.clone(),
                last4: self.last4.clone(),
                expiry_month: self.expiry_month,
                expiry_year: self.expiry_year,
            },
            VaultItemKind::Identity => VaultItemMetadata::Identity { label },
        };
        metadata.validate()?;
        Ok(metadata)
    }

    fn fields(&self) -> Result<Vec<String>, CliError> {
        let kind = VaultItemKind::from(self.kind);
        let fields = if self.fields.is_empty() {
            match kind {
                VaultItemKind::Login => vec!["password".to_owned()],
                VaultItemKind::ApiSecret => vec!["value".to_owned()],
                VaultItemKind::Card => vec!["number".to_owned()],
                VaultItemKind::Identity => {
                    return Err(CliError::VaultInput(
                        "identity items need at least one --field email|phone|address",
                    ));
                }
            }
        } else {
            self.fields.clone()
        };
        if let Some(field) = fields
            .iter()
            .find(|field| !kind.secret_fields().contains(&field.as_str()))
        {
            return Err(CliError::VaultField(field.clone(), label(kind)));
        }
        if (1..fields.len()).any(|index| fields[..index].contains(&fields[index])) {
            return Err(CliError::VaultInput("each --field may be given only once"));
        }
        Ok(fields)
    }
}

#[derive(Args)]
struct VaultRemoveArguments {
    #[arg(value_name = "ID|REF")]
    item: String,
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum ExternalProvider {
    Onepassword,
    Protonpass,
}

#[derive(Args)]
struct VaultConnectArguments {
    #[arg(value_enum)]
    provider: ExternalProvider,
}

#[derive(Serialize)]
struct ModelCredentialStatus {
    provider: String,
    source: ModelCredentialSource,
    storage: CredentialStorage,
    reference: uuid::Uuid,
    state: &'static str,
    management: &'static str,
}

#[derive(Serialize)]
struct VaultStatus {
    providers: Vec<VaultProviderState>,
    model_credentials: Vec<ModelCredentialStatus>,
}

pub async fn run(
    client: &HostClient,
    arguments: VaultArguments,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    match arguments.command {
        VaultCommand::Status(arguments) => status(client, arguments.json, cancellation).await,
        VaultCommand::List(arguments) => list(client, arguments, cancellation).await,
        VaultCommand::Add(arguments) => add(client, arguments, cancellation).await,
        VaultCommand::Remove(arguments) => remove(client, arguments, cancellation).await,
        VaultCommand::Connect(arguments) | VaultCommand::Disconnect(arguments) => {
            Err(match arguments.provider {
                ExternalProvider::Onepassword => {
                    VaultError::ProviderNotAvailable(VaultProviderId::OnePassword.as_str(), "#220")
                }
                ExternalProvider::Protonpass => {
                    VaultError::ProviderNotAvailable(VaultProviderId::ProtonPass.as_str(), "#221")
                }
            }
            .into())
        }
    }
}

async fn listing(
    client: &HostClient,
    limit: usize,
    cancellation: CancellationToken,
) -> Result<OwnerQueryResult, CliError> {
    Ok(client
        .query(OwnerQuery::VaultList { limit }, cancellation)
        .await?)
}

fn provider_line(state: &VaultProviderState) -> String {
    let detail = match &state.status {
        ProviderStatus::Ready { account } => format!("ready\t{account}"),
        ProviderStatus::Unavailable { reason } => format!("unavailable\t{reason}"),
        ProviderStatus::Disconnected => "disconnected".into(),
        ProviderStatus::Locked => "locked".into(),
    };
    format!("{}\t{detail}", state.provider.as_str())
}

async fn status(
    client: &HostClient,
    json: bool,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    let OwnerQueryResult::VaultList { listing } = listing(client, 1, cancellation.clone()).await?
    else {
        return Err(CliError::OwnerInput);
    };
    let OwnerQueryResult::Settings { settings } = client
        .query(OwnerQuery::Settings, cancellation.clone())
        .await?
    else {
        return Err(CliError::OwnerInput);
    };
    let mut model_credentials = Vec::new();
    for binding in settings.model_credentials {
        let state = match binding.source {
            ModelCredentialSource::ChatGpt => match client
                .operate(
                    OwnerOperation::ChatGpt {
                        operation: ChatGptOperation::Status {
                            binding: binding.clone(),
                        },
                    },
                    CommandId::new(),
                    cancellation.clone(),
                )
                .await
            {
                Ok(OwnerOperationResult::ChatGpt { status }) if status.signed_in => "signed_in",
                Ok(OwnerOperationResult::ChatGpt { .. }) => "signed_out",
                _ => "unknown",
            },
            ModelCredentialSource::ApiKey => "configured",
        };
        model_credentials.push(ModelCredentialStatus {
            provider: label(binding.provider),
            source: binding.source,
            storage: binding.storage,
            reference: binding.reference,
            state,
            management: "host_managed",
        });
    }
    let status = VaultStatus {
        providers: listing.providers,
        model_credentials,
    };
    if json {
        return write_json(status).await;
    }
    let mut lines: Vec<String> = status.providers.iter().map(provider_line).collect();
    lines.push(String::new());
    if status.model_credentials.is_empty() {
        lines.push("model credentials: none configured".into());
    } else {
        lines.push("model credentials (host-managed; never vault items, never listable or usable by tasks):".into());
    }
    lines.extend(status.model_credentials.iter().map(|credential| {
        format!(
            "{}\t{}\t{}\t{}",
            credential.provider,
            label(credential.source),
            label(credential.storage),
            credential.state,
        )
    }));
    print_lines(lines).await
}

async fn list(
    client: &HostClient,
    arguments: VaultListArguments,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    if !(1..=MAX_VAULT_LIST).contains(&arguments.limit) {
        return Err(CliError::VaultInput("--limit must be between 1 and 100"));
    }
    let result = listing(client, arguments.limit, cancellation).await?;
    if arguments.json {
        return write_json(result).await;
    }
    let OwnerQueryResult::VaultList { listing } = result else {
        return Err(CliError::OwnerInput);
    };
    for state in &listing.providers {
        if !matches!(state.status, ProviderStatus::Ready { .. }) {
            eprintln!("{}", provider_line(state));
        }
    }
    if listing.items.is_empty() {
        eprintln!("no vault items");
    }
    for item in &listing.incomplete {
        eprintln!(
            "incomplete {} {} ({}): {}; remove it with `everything vault remove {}`",
            label(item.kind),
            item.label,
            label(item.state),
            item.reference,
            item.reference.id()
        );
    }
    print_lines(
        listing
            .items
            .iter()
            .map(|item| {
                let mut detail = item.sites.join(",");
                if let Some(username) = &item.username {
                    detail = format!("{username}@{detail}");
                }
                match &item.metadata {
                    VaultItemMetadata::ApiSecret {
                        env_name: Some(name),
                        ..
                    } => detail = name.clone(),
                    VaultItemMetadata::Card {
                        last4: Some(last4), ..
                    } => detail = format!("…{last4}"),
                    _ => {}
                }
                format!(
                    "{}\t{}\t{}\t{}\t{}",
                    item.reference,
                    label(item.kind),
                    item.label,
                    item.fields.join(","),
                    if detail.is_empty() { "-" } else { &detail }
                )
            })
            .collect(),
    )
    .await
}

async fn add(
    client: &HostClient,
    arguments: VaultAddArguments,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    let metadata = arguments.metadata()?;
    let fields = arguments.fields()?;
    let secrets = tokio::task::spawn_blocking(move || {
        if io::stdin().is_terminal() {
            read_terminal(fields)
        } else {
            read_piped(fields)
        }
    })
    .await
    .map_err(|_| CliError::Worker)??;
    let item = VaultPutRequest { metadata, secrets };
    item.validate()?;
    let result = client
        .operate(
            OwnerOperation::VaultPut { item },
            CommandId::new(),
            cancellation,
        )
        .await?;
    if arguments.json {
        return write_json(result).await;
    }
    let OwnerOperationResult::VaultItemAdded { reference } = result else {
        return Err(CliError::OwnerInput);
    };
    print_lines(vec![reference.to_string()]).await
}

type Secrets = std::collections::BTreeMap<String, SecretTransfer>;

fn transfer(value: String) -> Result<SecretTransfer, CliError> {
    SecretTransfer::new(value).map_err(|_| {
        CliError::VaultInput("each secret must be 1-8192 bytes of printable UTF-8 text")
    })
}

fn read_piped(fields: Vec<String>) -> Result<Secrets, CliError> {
    let limit = fields.len() * (MAX_SECRET_BYTES + 2);
    let mut bytes = Zeroizing::new(Vec::with_capacity(limit + 1));
    io::stdin()
        .lock()
        .take(u64::try_from(limit + 1).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(CliError::VaultInput("piped secret input is too large"));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| CliError::VaultInput("piped secret input must be UTF-8 text"))?;
    let text = text.strip_suffix('\n').unwrap_or(text);
    let lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if lines.len() != fields.len() {
        return Err(CliError::VaultInput(
            "pipe exactly one secret line per --field, in order",
        ));
    }
    if lines.iter().any(|line| line.len() > MAX_SECRET_BYTES) {
        return Err(CliError::VaultInput("secret is longer than 8192 bytes"));
    }
    fields
        .into_iter()
        .zip(lines)
        .map(|(field, line)| Ok((field, transfer(line.to_owned())?)))
        .collect()
}

struct RawMode;

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

fn read_terminal(fields: Vec<String>) -> Result<Secrets, CliError> {
    fields
        .into_iter()
        .map(|field| {
            let value = read_hidden(&field)?;
            Ok((field, transfer(value)?))
        })
        .collect()
}

fn read_hidden(field: &str) -> Result<String, CliError> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
    let mut stderr = io::stderr();
    write!(stderr, "{field} (input hidden, Enter to finish): ")?;
    stderr.flush()?;
    let mut value = Zeroizing::new(String::with_capacity(MAX_SECRET_BYTES + 4));
    let mut overflow = false;
    let outcome = {
        crossterm::terminal::enable_raw_mode()?;
        let _raw = RawMode;
        loop {
            let Event::Key(key) = read()? else {
                continue;
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Enter => break Ok(()),
                KeyCode::Esc => break Err(CliError::VaultInput("secret entry cancelled")),
                KeyCode::Char('c' | 'd') if control => {
                    break Err(CliError::VaultInput("secret entry cancelled"));
                }
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Char(character) if !control && !character.is_control() => {
                    if value.len() + character.len_utf8() <= MAX_SECRET_BYTES {
                        value.push(character);
                    } else {
                        overflow = true;
                    }
                }
                _ => {}
            }
        }
    };
    writeln!(stderr)?;
    outcome?;
    if overflow {
        return Err(CliError::VaultInput("secret is longer than 8192 bytes"));
    }
    Ok(std::mem::take(&mut *value))
}

async fn remove(
    client: &HostClient,
    arguments: VaultRemoveArguments,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    let id = match arguments.item.parse::<uuid::Uuid>() {
        Ok(id) => id,
        Err(_) => arguments
            .item
            .parse::<VaultReference>()
            .and_then(|reference| reference.local_id())?,
    };
    let result = client
        .operate(
            OwnerOperation::VaultRemove { id },
            CommandId::new(),
            cancellation,
        )
        .await?;
    if arguments.json {
        return write_json(result).await;
    }
    print_lines(vec![format!("removed vault://local/{id}")]).await
}

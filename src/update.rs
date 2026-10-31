use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::service::ServiceError;

const REPOSITORY: &str = "izyuumi/everything";
const DEFAULT_BASE_URL: &str = "https://api.github.com";
const BASE_URL_VARIABLE: &str = "EVERYTHING_UPDATE_BASE_URL";
const MAX_METADATA_BYTES: usize = 4 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: usize = 512 * 1024 * 1024;

#[derive(Args)]
pub struct UpdateArguments {
    #[arg(long)]
    pub beta: bool,
    #[arg(long)]
    pub check: bool,
    #[arg(long)]
    pub allow_unsigned: bool,
}

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("no release asset exists for this platform")]
    UnsupportedPlatform,
    #[error("invalid version {0:?}")]
    Version(String),
    #[error("could not initialize the HTTP client: {0}")]
    Client(#[source] reqwest::Error),
    #[error("request to {url} failed: {source}")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("{url} returned HTTP {status}")]
    Status { url: String, status: u16 },
    #[error("{url} exceeded {limit} bytes")]
    TooLarge { url: String, limit: usize },
    #[error("{url} is not valid release metadata: {source}")]
    Json {
        url: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("no {channel} release found at {url}")]
    NoRelease { channel: Channel, url: String },
    #[error("release {tag} has no asset {asset}")]
    MissingAsset { tag: String, asset: String },
    #[error("release {tag} candidate.json names version {version}, which does not match the tag")]
    CandidateMismatch { tag: String, version: String },
    #[error("SHA256SUMS of release {tag} has no entry for {asset}")]
    MissingDigest { tag: String, asset: String },
    #[error(
        "{asset} from {url} does not match SHA256SUMS (expected {expected}, got {actual}); refusing to install"
    )]
    DigestMismatch {
        asset: String,
        url: String,
        expected: String,
        actual: String,
    },
    #[error("could not extract everything from {asset}: {source}")]
    Extract {
        asset: String,
        #[source]
        source: self_update::errors::Error,
    },
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not resolve the running executable: {0}")]
    CurrentExecutable(#[source] std::io::Error),
    #[error("{} reports {actual:?}, expected \"everything {expected}\"", path.display())]
    UnexpectedBinary {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("codesign could not inspect {}: {detail}", path.display())]
    Codesign { path: PathBuf, detail: String },
    #[error("could not run codesign on {}: {source}", path.display())]
    CodesignSpawn {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    SignaturePolicy(String),
    #[error(
        "everything {version} was installed, but restarting the launchd service failed: {source}; restart it with `everything service stop` and `everything service start`"
    )]
    ServiceRestart {
        version: String,
        #[source]
        source: ServiceError,
    },
    #[error("update was cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, Copy)]
pub enum Channel {
    Stable,
    Beta,
}

impl fmt::Display for Channel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    core: (u64, u64, u64),
    build: u64,
}

impl Version {
    fn parse(core: &str, build: u64) -> Result<Self, UpdateError> {
        let invalid = || UpdateError::Version(core.to_owned());
        let mut parts = core.split('.').map(|part| {
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(invalid());
            }
            part.parse::<u64>().map_err(|_| invalid())
        });
        let version = (
            parts.next().ok_or_else(invalid)??,
            parts.next().ok_or_else(invalid)??,
            parts.next().ok_or_else(invalid)??,
        );
        if parts.next().is_some() {
            return Err(invalid());
        }
        Ok(Self {
            core: version,
            build,
        })
    }

    fn running(text: &str) -> Result<Self, UpdateError> {
        match text.split_once('+') {
            Some((core, build)) => Self::parse(
                core,
                build
                    .parse()
                    .map_err(|_| UpdateError::Version(text.to_owned()))?,
            ),
            None => Self::parse(text, 0),
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (major, minor, patch) = self.core;
        write!(formatter, "{major}.{minor}.{patch}")?;
        if self.build > 0 {
            write!(formatter, "+{}", self.build)?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

#[derive(Deserialize)]
struct Candidate {
    version: String,
    build: u64,
}

impl Release {
    fn asset(&self, name: &str) -> Result<&str, UpdateError> {
        self.assets
            .iter()
            .find(|asset| asset.name == name)
            .map(|asset| asset.browser_download_url.as_str())
            .ok_or_else(|| UpdateError::MissingAsset {
                tag: self.tag_name.clone(),
                asset: name.to_owned(),
            })
    }

    fn stable_version(&self) -> Option<Version> {
        let core = self.tag_name.strip_prefix('v')?;
        Version::parse(core, 0).ok()
    }
}

pub async fn run(
    arguments: UpdateArguments,
    running_version: &str,
    state_directory: PathBuf,
    cancellation: CancellationToken,
) -> Result<(), UpdateError> {
    let channel = if arguments.beta {
        Channel::Beta
    } else {
        Channel::Stable
    };
    let current = Version::running(running_version)?;
    let target = target_triple().ok_or(UpdateError::UnsupportedPlatform)?;
    let client = reqwest::Client::builder()
        .user_agent(concat!("everything-update/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .build()
        .map_err(UpdateError::Client)?;
    let base = std::env::var(BASE_URL_VARIABLE).unwrap_or_else(|_| DEFAULT_BASE_URL.to_owned());
    let releases_url = format!(
        "{}/repos/{REPOSITORY}/releases?per_page=100",
        base.trim_end_matches('/')
    );
    let releases: Vec<Release> = json(
        &releases_url,
        &fetch(&client, &releases_url, MAX_METADATA_BYTES, &cancellation).await?,
    )?;
    let release = match channel {
        Channel::Beta => releases.iter().find(|release| {
            !release.draft
                && release.tag_name.starts_with('v')
                && release.tag_name.ends_with("-beta")
        }),
        Channel::Stable => releases
            .iter()
            .filter(|release| !release.draft && !release.prerelease)
            .filter_map(|release| Some((release.stable_version()?, release)))
            .max_by_key(|(version, _)| *version)
            .map(|(_, release)| release),
    }
    .ok_or_else(|| UpdateError::NoRelease {
        channel,
        url: releases_url.clone(),
    })?;
    let candidate_url = release.asset("candidate.json")?;
    let candidate: Candidate = json(
        candidate_url,
        &fetch(&client, candidate_url, MAX_METADATA_BYTES, &cancellation).await?,
    )?;
    let tag_version = release
        .tag_name
        .trim_start_matches('v')
        .trim_end_matches("-beta");
    if candidate.version != tag_version {
        return Err(UpdateError::CandidateMismatch {
            tag: release.tag_name.clone(),
            version: candidate.version,
        });
    }
    let available = Version::parse(&candidate.version, candidate.build)?;
    let newer = available > current;
    println!(
        "channel={channel} current={current} available={available} tag={} status={}",
        release.tag_name,
        if newer { "newer" } else { "up-to-date" }
    );
    if !newer {
        println!("everything {current} is up to date on the {channel} channel.");
        return Ok(());
    }
    if arguments.check {
        println!(
            "everything {available} ({}) is available; run `everything update{}` to install it.",
            release.tag_name,
            if arguments.beta { " --beta" } else { "" }
        );
        return Ok(());
    }

    let asset = format!("everything-{target}.tar.gz");
    let archive_url = release.asset(&asset)?;
    let sums_url = release.asset("SHA256SUMS")?;
    let sums = fetch(&client, sums_url, MAX_METADATA_BYTES, &cancellation).await?;
    let expected = String::from_utf8_lossy(&sums)
        .lines()
        .filter_map(|line| line.split_once("  "))
        .find(|(_, name)| name.trim_start_matches('*') == asset)
        .map(|(digest, _)| digest.to_ascii_lowercase())
        .ok_or_else(|| UpdateError::MissingDigest {
            tag: release.tag_name.clone(),
            asset: asset.clone(),
        })?;
    let archive = fetch(&client, archive_url, MAX_ARCHIVE_BYTES, &cancellation).await?;
    let actual = hex(&Sha256::digest(&archive));
    if actual != expected {
        return Err(UpdateError::DigestMismatch {
            asset,
            url: archive_url.to_owned(),
            expected,
            actual,
        });
    }

    let executable = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(UpdateError::CurrentExecutable)?;
    let work = WorkDirectory::create()?;
    let archive_path = work.path.join(&asset);
    std::fs::write(&archive_path, &archive).map_err(io_error(&archive_path))?;
    self_update::Extract::from_source(&archive_path)
        .archive(self_update::ArchiveKind::Tar(Some(
            self_update::Compression::Gz,
        )))
        .extract_file(&work.path, "everything")
        .map_err(|source| UpdateError::Extract {
            asset: asset.clone(),
            source,
        })?;
    let replacement = work.path.join("everything");
    check_signatures(&executable, &replacement, &asset, arguments.allow_unsigned).await?;
    check_version(&replacement, &available, &cancellation).await?;
    if cancellation.is_cancelled() {
        return Err(UpdateError::Cancelled);
    }
    replace(&executable, &replacement)?;
    println!(
        "Updated {} from {current} to {available} ({}).",
        executable.display(),
        release.tag_name
    );
    restart_service(&executable, state_directory, cancellation)
        .await
        .map_err(|source| UpdateError::ServiceRestart {
            version: available.to_string(),
            source,
        })
}

fn target_triple() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(
        target_os = "linux",
        target_env = "gnu",
        target_arch = "x86_64"
    )) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(
        target_os = "linux",
        target_env = "gnu",
        target_arch = "aarch64"
    )) {
        Some("aarch64-unknown-linux-gnu")
    } else {
        None
    }
}

async fn fetch(
    client: &reqwest::Client,
    url: &str,
    limit: usize,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, UpdateError> {
    let http = |source| UpdateError::Http {
        url: url.to_owned(),
        source,
    };
    let request = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send();
    let mut response = tokio::select! {
        _ = cancellation.cancelled() => return Err(UpdateError::Cancelled),
        response = request => response.map_err(http)?,
    };
    if !response.status().is_success() {
        return Err(UpdateError::Status {
            url: url.to_owned(),
            status: response.status().as_u16(),
        });
    }
    let too_large = || UpdateError::TooLarge {
        url: url.to_owned(),
        limit,
    };
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    loop {
        let chunk = tokio::select! {
            _ = cancellation.cancelled() => return Err(UpdateError::Cancelled),
            chunk = response.chunk() => chunk.map_err(http)?,
        };
        let Some(chunk) = chunk else {
            return Ok(body);
        };
        if body.len() + chunk.len() > limit {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
}

fn json<T: serde::de::DeserializeOwned>(url: &str, body: &[u8]) -> Result<T, UpdateError> {
    serde_json::from_slice(body).map_err(|source| UpdateError::Json {
        url: url.to_owned(),
        source,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> UpdateError + '_ {
    move |source| UpdateError::Io {
        path: path.to_owned(),
        source,
    }
}

struct WorkDirectory {
    path: PathBuf,
}

impl WorkDirectory {
    fn create() -> Result<Self, UpdateError> {
        use std::os::unix::fs::DirBuilderExt;
        let path = std::env::temp_dir().join(format!("everything-update-{}", uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(io_error(&path))?;
        Ok(Self { path })
    }
}

impl Drop for WorkDirectory {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.path) {
            tracing::warn!(path = %self.path.display(), %error, "could not remove update work directory");
        }
    }
}

async fn check_version(
    binary: &Path,
    expected: &Version,
    cancellation: &CancellationToken,
) -> Result<(), UpdateError> {
    let output = tokio::process::Command::new(binary)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::select! {
        _ = cancellation.cancelled() => return Err(UpdateError::Cancelled),
        output = tokio::time::timeout(Duration::from_secs(15), output) => output,
    };
    let actual = match output {
        Ok(Ok(output)) => String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        Ok(Err(source)) => return Err(io_error(binary)(source)),
        Err(_) => "no answer within 15 seconds".to_owned(),
    };
    if actual == format!("everything {expected}") {
        Ok(())
    } else {
        Err(UpdateError::UnexpectedBinary {
            path: binary.to_owned(),
            expected: expected.to_string(),
            actual,
        })
    }
}

fn replace(executable: &Path, replacement: &Path) -> Result<(), UpdateError> {
    use std::os::unix::fs::OpenOptionsExt;
    let directory = executable.parent().ok_or_else(|| UpdateError::Io {
        path: executable.to_owned(),
        source: std::io::Error::other("executable has no parent directory"),
    })?;
    let temporary = directory.join(format!(".everything.update.{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(&temporary)
            .map_err(io_error(&temporary))?;
        let mut source = std::fs::File::open(replacement).map_err(io_error(replacement))?;
        std::io::copy(&mut source, &mut file).map_err(io_error(&temporary))?;
        file.sync_all().map_err(io_error(&temporary))?;
        std::fs::rename(&temporary, executable).map_err(io_error(executable))
    })();
    if result.is_err()
        && let Err(error) = std::fs::remove_file(&temporary)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %temporary.display(), %error, "could not remove update temporary file");
    }
    result
}

#[cfg(target_os = "macos")]
async fn restart_service(
    executable: &Path,
    state_directory: PathBuf,
    cancellation: CancellationToken,
) -> Result<(), ServiceError> {
    let launch_agents = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/LaunchAgents"))
        .ok_or(ServiceError::UnsafePath("HOME is unavailable"))?;
    let service = crate::service::LaunchdService::new(executable, state_directory, launch_agents)?;
    if service.restart_if_running(cancellation).await? {
        println!("Restarted launchd service {}.", service.label());
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
async fn restart_service(
    _executable: &Path,
    _state_directory: PathBuf,
    _cancellation: CancellationToken,
) -> Result<(), ServiceError> {
    Ok(())
}

#[cfg(target_os = "macos")]
const SIGNING_IDENTIFIER: &str = "to.yumi.everything.cli";

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct Signature {
    identifier: Option<String>,
    team: Option<String>,
    adhoc: bool,
    valid: bool,
    requirement: Option<String>,
}

#[cfg(target_os = "macos")]
impl Signature {
    fn describe(signature: Option<&Self>) -> String {
        match signature {
            None => "unsigned".to_owned(),
            Some(signature) if signature.adhoc => "ad-hoc signed".to_owned(),
            Some(signature) if signature.team.is_none() => {
                "signed without a team identifier".to_owned()
            }
            Some(signature) => format!(
                "signed by team {} as {}{}",
                signature.team.as_deref().unwrap_or_default(),
                signature.identifier.as_deref().unwrap_or("(none)"),
                if signature.valid {
                    ""
                } else {
                    ", failing strict verification"
                }
            ),
        }
    }
}

#[cfg(target_os = "macos")]
async fn codesign(arguments: &[&str], path: &Path) -> Result<std::process::Output, UpdateError> {
    tokio::process::Command::new("/usr/bin/codesign")
        .args(arguments)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|source| UpdateError::CodesignSpawn {
            path: path.to_owned(),
            source,
        })
}

#[cfg(target_os = "macos")]
async fn inspect(path: &Path) -> Result<Option<Signature>, UpdateError> {
    let details = codesign(&["-dv", "--verbose=4"], path).await?;
    let text = String::from_utf8_lossy(&details.stderr);
    if !details.status.success() {
        if text.contains("not signed at all") {
            return Ok(None);
        }
        return Err(UpdateError::Codesign {
            path: path.to_owned(),
            detail: text.trim().to_owned(),
        });
    }
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .map(str::to_owned)
    };
    let team = field("TeamIdentifier=").filter(|team| team != "not set");
    let adhoc = field("Signature=").is_some_and(|value| value == "adhoc");
    let valid = codesign(&["--verify", "--strict"], path)
        .await?
        .status
        .success();
    let requirement = codesign(&["-d", "-r-"], path).await?;
    let requirement = String::from_utf8_lossy(&requirement.stdout)
        .lines()
        .find_map(|line| line.trim().strip_prefix("designated => "))
        .map(str::to_owned);
    Ok(Some(Signature {
        identifier: field("Identifier="),
        team,
        adhoc,
        valid,
        requirement,
    }))
}

#[cfg(target_os = "macos")]
async fn check_signatures(
    executable: &Path,
    replacement: &Path,
    asset: &str,
    allow_unsigned: bool,
) -> Result<(), UpdateError> {
    let current = inspect(executable).await?;
    let new = inspect(replacement).await?;
    let new_description = Signature::describe(new.as_ref());
    let Some((team, current)) = current
        .as_ref()
        .filter(|signature| !signature.adhoc)
        .and_then(|signature| Some((signature.team.as_deref()?, signature)))
    else {
        println!(
            "Note: {} is {}, so no signature policy applies; the update is {new_description}.",
            executable.display(),
            Signature::describe(current.as_ref())
        );
        return Ok(());
    };
    let acceptable = new.as_ref().is_some_and(|signature| {
        signature.valid
            && !signature.adhoc
            && signature.team.as_deref() == Some(team)
            && signature.identifier.as_deref() == Some(SIGNING_IDENTIFIER)
    });
    if !acceptable {
        let message = format!(
            "refusing to replace {} ({}) with {asset} ({new_description}): macOS keychain items such as the Sign in with ChatGPT session trust the current signature, so the updated binary could not read them. Install a release signed by team {team} as {SIGNING_IDENTIFIER} (re-sign a local build with scripts/sign_macos.sh), or re-run with --allow-unsigned to install it anyway and sign in again afterwards.",
            executable.display(),
            Signature::describe(Some(current)),
        );
        if !allow_unsigned {
            return Err(UpdateError::SignaturePolicy(message));
        }
        eprintln!("Warning: --allow-unsigned overrides: {message}");
        return Ok(());
    }
    if new
        .as_ref()
        .and_then(|signature| signature.requirement.as_ref())
        != current.requirement.as_ref()
    {
        eprintln!(
            "Warning: the update is signed by team {team} with a different certificate than {}; keychain items it saved, including the Sign in with ChatGPT session, may need signing in again.",
            executable.display()
        );
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
async fn check_signatures(
    _executable: &Path,
    _replacement: &Path,
    _asset: &str,
    _allow_unsigned: bool,
) -> Result<(), UpdateError> {
    Ok(())
}

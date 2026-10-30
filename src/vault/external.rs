use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Component, Path};
use std::process::Stdio;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::{VaultError, VaultItemSummary, VaultProviderId, VaultReference};

const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_STDERR_BYTES: usize = 4_096;
const MAX_PATH_BYTES: usize = 4_096;
pub(crate) const MAX_CACHED_ITEMS: usize = 1_024;

static ITEMS: Mutex<BTreeMap<VaultReference, VaultItemSummary>> = Mutex::new(BTreeMap::new());

pub(crate) struct ProviderCli {
    pub(crate) provider: &'static str,
    pub(crate) program: &'static str,
    pub(crate) classify: fn(&str) -> Option<VaultError>,
}

impl ProviderCli {
    pub(crate) async fn run(
        &self,
        binary: &Path,
        arguments: &[&str],
        environment: &[(&str, &OsStr)],
        limit: usize,
        cancellation: &CancellationToken,
    ) -> Result<Zeroizing<Vec<u8>>, VaultError> {
        let mut command = tokio::process::Command::new(binary);
        command
            .args(arguments)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(home) = std::env::var_os("HOME") {
            command.env("HOME", home);
        }
        for (key, value) in environment {
            command.env(key, value);
        }
        let mut child = command.spawn().map_err(|_| {
            self.failed(&format!(
                "the pinned {} binary could not be started",
                self.program
            ))
        })?;
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(self.failed(&format!("the {} pipes could not be opened", self.program)));
        };
        let work = async {
            let mut output = Zeroizing::new(Vec::with_capacity(limit + 1));
            let mut errors = Zeroizing::new(Vec::with_capacity(MAX_STDERR_BYTES + 1));
            let mut stdout = stdout.take(u64::try_from(limit + 1).unwrap_or(u64::MAX));
            let mut stderr = stderr.take(u64::try_from(MAX_STDERR_BYTES).unwrap_or(u64::MAX));
            let (read, _) = tokio::join!(
                stdout.read_to_end(&mut output),
                stderr.read_to_end(&mut errors),
            );
            read.map_err(|_| self.failed(&format!("reading {} output failed", self.program)))?;
            if output.len() > limit {
                return Err(
                    self.failed(&format!("{} output exceeded its size limit", self.program))
                );
            }
            let status = child
                .wait()
                .await
                .map_err(|_| self.failed(&format!("waiting for {} failed", self.program)))?;
            if !status.success() {
                let text = Zeroizing::new(String::from_utf8_lossy(&errors).to_lowercase());
                return Err((self.classify)(&text).unwrap_or_else(|| {
                    self.failed(&match status.code() {
                        Some(code) => format!("{} exited with status {code}", self.program),
                        None => format!("{} was terminated by a signal", self.program),
                    })
                }));
            }
            Ok(output)
        };
        tokio::select! {
            () = cancellation.cancelled() => Err(VaultError::Cancelled),
            () = tokio::time::sleep(TIMEOUT) => Err(self.failed(&format!("{} timed out after 10 seconds", self.program))),
            result = work => result,
        }
    }

    pub(crate) fn executable(&self, binary: &Path) -> Result<(), VaultError> {
        use std::os::unix::fs::PermissionsExt;
        if !valid_binary_path(binary) {
            return Err(VaultError::InvalidItem(
                "the provider binary must be an absolute path without relative components",
            ));
        }
        match std::fs::metadata(binary) {
            Ok(metadata) if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 => {
                Ok(())
            }
            _ => Err(self.failed(&format!(
                "the pinned {} binary {} is missing or not executable",
                self.program,
                binary.display()
            ))),
        }
    }

    pub(crate) fn failed(&self, reason: &str) -> VaultError {
        VaultError::ProviderFailed(self.provider, reason.to_owned())
    }

    pub(crate) fn malformed(&self) -> VaultError {
        self.failed(&format!(
            "{} returned output in an unexpected shape",
            self.program
        ))
    }
}

pub(crate) fn valid_binary_path(path: &Path) -> bool {
    path.is_absolute()
        && path
            .to_str()
            .is_some_and(|text| text.len() <= MAX_PATH_BYTES && !text.chars().any(char::is_control))
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

pub(crate) fn cached_summary(reference: &VaultReference) -> Result<VaultItemSummary, VaultError> {
    let key = VaultReference::from_parts(reference.provider(), reference.id(), None)?;
    ITEMS
        .lock()
        .map_err(|_| VaultError::Worker)?
        .get(&key)
        .cloned()
        .ok_or(VaultError::NotFound)
}

fn cache() -> MutexGuard<'static, BTreeMap<VaultReference, VaultItemSummary>> {
    ITEMS.lock().unwrap_or_else(|poisoned| {
        let mut items = poisoned.into_inner();
        items.clear();
        ITEMS.clear_poison();
        items
    })
}

pub(crate) fn remember(provider: VaultProviderId, prefix: &str, summaries: &[VaultItemSummary]) {
    let mut items = cache();
    items.retain(|reference, _| {
        reference.provider() != provider || !reference.id().starts_with(prefix)
    });
    for summary in summaries {
        if items.len() >= MAX_CACHED_ITEMS {
            break;
        }
        items.insert(summary.reference.clone(), summary.clone());
    }
}

pub(crate) fn forget(provider: VaultProviderId, prefix: &str) {
    cache().retain(|reference, _| {
        reference.provider() != provider || !reference.id().starts_with(prefix)
    });
}

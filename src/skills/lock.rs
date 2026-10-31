use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::SkillDirName;

const MAX_LOCK_BYTES: u64 = 4 * 1024 * 1024;
const GLOBAL_LOCK_VERSION: u64 = 3;
const PROJECT_LOCK_VERSION: u64 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockEntry {
    pub source: String,
    pub source_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_folder_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computed_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_name: Option<String>,
}

#[derive(Default)]
pub(super) struct Lock {
    pub entries: BTreeMap<String, LockEntry>,
}

impl Lock {
    pub fn find(&self, name: &str) -> Option<(&String, &LockEntry)> {
        self.entries.get_key_value(name).or_else(|| {
            let wanted = SkillDirName::sanitize(name);
            self.entries
                .iter()
                .find(|(key, _)| SkillDirName::sanitize(key) == wanted)
        })
    }
}

#[derive(Deserialize)]
struct RawLock {
    version: Option<serde_json::Value>,
    skills: Option<BTreeMap<String, serde_json::Value>>,
}

pub(super) fn global_lock_path(home: &Path) -> PathBuf {
    match std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        Some(state) => PathBuf::from(state).join("skills").join(".skill-lock.json"),
        None => home.join(".agents").join(".skill-lock.json"),
    }
}

pub(super) fn read_global(home: &Path, warnings: &mut Vec<String>) -> Lock {
    read(&global_lock_path(home), GLOBAL_LOCK_VERSION, true, warnings)
}

pub(super) fn read_project(workspace: &Path, warnings: &mut Vec<String>) -> Lock {
    read(
        &workspace.join("skills-lock.json"),
        PROJECT_LOCK_VERSION,
        false,
        warnings,
    )
}

fn read(path: &Path, version: u64, minimum: bool, warnings: &mut Vec<String>) -> Lock {
    let bytes = match read_bounded(path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Lock::default(),
        Err(error) => {
            warnings.push(format!(
                "lock {} could not be read: {error}; sources unknown",
                path.display()
            ));
            return Lock::default();
        }
    };
    let raw: RawLock = match serde_json::from_slice(&bytes) {
        Ok(raw) => raw,
        Err(error) => {
            warnings.push(format!(
                "lock {} is not valid JSON ({error}); read as empty",
                path.display()
            ));
            return Lock::default();
        }
    };
    let found = raw.version.as_ref().and_then(serde_json::Value::as_u64);
    let supported = match found {
        Some(found) if minimum => found >= version,
        Some(found) => found == version,
        None => false,
    };
    let Some(skills) = raw.skills.filter(|_| supported) else {
        warnings.push(format!(
            "lock {} has unsupported version {}; read as empty",
            path.display(),
            raw.version
                .map_or_else(|| "none".to_owned(), |value| value.to_string())
        ));
        return Lock::default();
    };
    let mut entries = BTreeMap::new();
    for (key, value) in skills {
        match serde_json::from_value::<LockEntry>(value) {
            Ok(entry) => {
                entries.insert(key, entry);
            }
            Err(error) => warnings.push(format!(
                "lock {} entry {key:?} is malformed ({error}); ignored",
                path.display()
            )),
        }
    }
    Lock { entries }
}

fn read_bounded(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    if metadata.len() > MAX_LOCK_BYTES {
        return Err(io::Error::other("larger than 4 MiB"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_LOCK_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_LOCK_BYTES {
        return Err(io::Error::other("larger than 4 MiB"));
    }
    Ok(Some(bytes))
}

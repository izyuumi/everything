use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags};
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

pub(super) type RawEntries = serde_json::Map<String, serde_json::Value>;

pub(super) fn at(path: &Path) -> impl FnOnce(io::Error) -> io::Error + '_ {
    move |error| io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

pub(super) fn read_global_raw(home: &Path) -> io::Result<RawEntries> {
    let path = global_lock_path(home);
    let bytes = read_bounded(&path).map_err(at(&path))?;
    let document = writable(bytes.as_deref()).map_err(at(&path))?;
    Ok(match document.get("skills") {
        Some(serde_json::Value::Object(skills)) => skills.clone(),
        _ => RawEntries::new(),
    })
}

pub(super) fn write_global(home: &Path, mut change: impl FnMut(&mut RawEntries)) -> io::Result<()> {
    let path = global_lock_path(home);
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("lock path has no parent"))?;
    std::fs::create_dir_all(parent).map_err(at(parent))?;
    for _ in 0..2 {
        let before = read_bounded(&path).map_err(at(&path))?;
        let mut document = writable(before.as_deref()).map_err(at(&path))?;
        let skills = document
            .entry("skills")
            .or_insert_with(|| serde_json::Value::Object(RawEntries::new()));
        let serde_json::Value::Object(skills) = skills else {
            return Err(io::Error::other(format!(
                "lock {} has a non-object skills field; fix or remove it",
                path.display()
            )));
        };
        change(skills);
        let bytes = serde_json::to_vec_pretty(&document).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_LOCK_BYTES {
            return Err(at(&path)(io::Error::other("lock would exceed 4 MiB")));
        }
        let temporary = parent.join(format!(".skill-lock.json.vorra-{}", uuid::Uuid::new_v4()));
        let written = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            io::Write::write_all(&mut file, &bytes)?;
            file.sync_all()
        })();
        if let Err(error) = written {
            let _ = std::fs::remove_file(&temporary);
            return Err(at(&temporary)(error));
        }
        if read_bounded(&path).map_err(at(&path))? == before {
            return std::fs::rename(&temporary, &path)
                .inspect_err(|_| {
                    let _ = std::fs::remove_file(&temporary);
                })
                .map_err(at(&path));
        }
        let _ = std::fs::remove_file(&temporary);
    }
    Err(io::Error::other(format!(
        "lock {} changed twice while Vorra was writing it; nothing was written, run the command again",
        path.display()
    )))
}

fn writable(bytes: Option<&[u8]>) -> io::Result<RawEntries> {
    let Some(bytes) = bytes else {
        return Ok(
            serde_json::json!({"version": GLOBAL_LOCK_VERSION, "skills": {}, "dismissed": {}})
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
    };
    let document: serde_json::Value = serde_json::from_slice(bytes).map_err(|error| {
        io::Error::other(format!(
            "global lock is not valid JSON ({error}); Vorra does not overwrite it, fix or remove it"
        ))
    })?;
    let serde_json::Value::Object(document) = document else {
        return Err(io::Error::other(
            "global lock is not a JSON object; Vorra does not overwrite it",
        ));
    };
    match document.get("version").and_then(serde_json::Value::as_u64) {
        Some(version) if version >= GLOBAL_LOCK_VERSION => Ok(document),
        _ => Err(io::Error::other(
            "global lock version is below 3; run `npx skills list -g` to migrate it first",
        )),
    }
}

fn read_bounded(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let file = match rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(file) => File::from(file),
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP) => return Err(io::Error::other("is a symlink")),
        Err(error) => return Err(error.into()),
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

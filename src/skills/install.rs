use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use chrono::{SecondsFormat, Utc};
use rustix::fs::{FlockOperation, Mode, OFlags};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use super::cli::{AddArguments, RemoveArguments, SkillsCliError, UpdateArguments, shown};
use super::discover::read_skill_file;
use super::fetch::{self, Checkout, FoundSkill, Source};
use super::lock::at;
use super::{Catalog, LockEntry, SkillDirName, SkillRoot, lock};

const MAX_COPY_ENTRIES: usize = 4096;
const MAX_COPY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_COPY_DEPTH: usize = 16;
const COPY_EXCLUDED_DIRS: [&str; 3] = [".git", "__pycache__", "__pypackages__"];
const HASH_EXCLUDED_DIRS: [&str; 2] = [".git", "node_modules"];
const NPX_UPDATE: &str = "npx skills update -g -y";

pub(super) struct Staging {
    home: PathBuf,
    skills: PathBuf,
    root: PathBuf,
    _lock: OwnedFd,
}

fn staging_root(home: &Path) -> PathBuf {
    home.join(".agents").join(".vorra-staging")
}

fn present(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

pub(super) fn recover_if_idle(home: &Path) {
    if !present(&staging_root(home)) {
        return;
    }
    match Staging::open(home) {
        Ok(_) | Err(SkillsCliError::Busy) => {}
        Err(error) => eprintln!(
            "vorra: warning: interrupted skills staging was not recovered: {}",
            shown(error)
        ),
    }
}

impl Staging {
    pub fn open(home: &Path) -> Result<Self, SkillsCliError> {
        let skills = home.join(".agents").join("skills");
        fs::create_dir_all(&skills).map_err(at(&skills))?;
        let root = staging_root(home);
        match fs::DirBuilder::new().mode(0o700).create(&root) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(at(&root)(error).into()),
        }
        let metadata = fs::symlink_metadata(&root).map_err(at(&root))?;
        if !metadata.is_dir()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
        {
            return Err(SkillsCliError::Failed(format!(
                "{} must be a directory owned by you with mode 0700",
                root.display()
            )));
        }
        let lock_path = root.join(".lock");
        let lock = rustix::fs::open(
            &lock_path,
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|error| at(&lock_path)(error.into()))?;
        match rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(rustix::io::Errno::WOULDBLOCK) => return Err(SkillsCliError::Busy),
            Err(error) => return Err(at(&lock_path)(error.into()).into()),
        }
        let staging = Self {
            home: home.to_owned(),
            skills,
            root,
            _lock: lock,
        };
        staging.recover()?;
        Ok(staging)
    }

    fn recover(&self) -> Result<(), SkillsCliError> {
        for entry in fs::read_dir(&self.root).map_err(at(&self.root))? {
            let entry = entry.map_err(at(&self.root))?;
            if !entry.file_type().map_err(at(&entry.path()))?.is_dir() {
                continue;
            }
            let stage = entry.path();
            let Some(name) = target_name(&stage) else {
                if present(&stage.join("old")) || present(&stage.join("swapped")) {
                    eprintln!(
                        "vorra: warning: {} holds a skill with no recorded target; left in place",
                        shown(stage.display())
                    );
                } else {
                    discard_stage(&stage);
                }
                continue;
            };
            let target = self.skills.join(&name);
            match roll_back(&stage, &target, &self.home) {
                Ok(true) => eprintln!(
                    "vorra: restored {} from an interrupted skills command",
                    shown(target.display())
                ),
                Ok(false) => {}
                Err(error) => {
                    eprintln!(
                        "vorra: warning: interrupted change to {} was not rolled back ({}); its previous version stays in {}",
                        shown(target.display()),
                        shown(error),
                        shown(stage.display())
                    );
                    continue;
                }
            }
            discard_stage(&stage);
        }
        Ok(())
    }

    pub fn replace(
        &self,
        folder: &str,
        source: Option<&Path>,
        expected: &Expected,
        commit: impl FnOnce() -> Result<(), SkillsCliError>,
    ) -> Result<PathBuf, SkillsCliError> {
        let stage = self.root.join(uuid::Uuid::new_v4().to_string());
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&stage)
            .map_err(at(&stage))?;
        let target = self.skills.join(folder);
        let result = swap(&stage, folder, &target, source, expected, commit);
        if result.is_err()
            && (present(&stage.join("old")) || present(&stage.join("swapped")))
            && let Err(error) = roll_back(&stage, &target, &self.home)
        {
            eprintln!(
                "vorra: warning: rollback of {} failed ({}); its previous version stays in {}",
                shown(target.display()),
                shown(error),
                shown(stage.display())
            );
            return result;
        }
        discard_stage(&stage);
        result
    }
}

pub(super) enum Expected<'a> {
    Hash {
        key: &'a str,
        hash: &'a str,
        updated_at: &'a str,
    },
    Absent {
        key: &'a str,
    },
}

fn swap(
    stage: &Path,
    folder: &str,
    target: &Path,
    source: Option<&Path>,
    expected: &Expected,
    commit: impl FnOnce() -> Result<(), SkillsCliError>,
) -> Result<PathBuf, SkillsCliError> {
    let marker = |name: &str| {
        let path = stage.join(name);
        fs::write(&path, b"").map_err(at(&path))
    };
    let recorded = stage.join("target");
    fs::write(&recorded, folder).map_err(at(&recorded))?;
    let expectation = match expected {
        Expected::Hash {
            key,
            hash,
            updated_at,
        } => json!({ "key": key, "hash": hash, "updatedAt": updated_at }),
        Expected::Absent { key } => json!({ "key": key }),
    };
    let expect_path = stage.join("expect");
    fs::write(&expect_path, expectation.to_string()).map_err(at(&expect_path))?;
    let new = stage.join("new");
    let old = stage.join("old");
    if let Some(source) = source {
        copy_tree(source, &new, &mut Budget::default(), 0)?;
    }
    match fs::rename(target, &old) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(at(target)(error).into()),
    }
    if source.is_some() {
        marker("swapped")?;
        fs::rename(&new, target).map_err(at(target))?;
    }
    commit()?;
    marker("committed")?;
    if present(&old) {
        let discard = stage.join("discard");
        fs::rename(&old, &discard).map_err(at(&old))?;
    }
    let swapped = stage.join("swapped");
    if present(&swapped) {
        fs::remove_file(&swapped).map_err(at(&swapped))?;
    }
    Ok(target.to_owned())
}

fn committed(stage: &Path, home: &Path) -> io::Result<bool> {
    if present(&stage.join("committed")) {
        return Ok(true);
    }
    let path = stage.join("expect");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(at(&path)(error)),
    };
    let expectation: Value =
        serde_json::from_slice(&bytes).map_err(|error| at(&path)(io::Error::other(error)))?;
    let key = expectation
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| at(&path)(io::Error::other("no lock key recorded")))?;
    let skills = lock::read_global_raw(home)?;
    let entry = skills.get(key);
    Ok(match expectation.get("hash").and_then(Value::as_str) {
        Some(hash) => {
            let field = |name: &str| {
                entry
                    .and_then(|entry| entry.get(name))
                    .and_then(Value::as_str)
            };
            field("skillFolderHash") == Some(hash)
                && expectation
                    .get("updatedAt")
                    .and_then(Value::as_str)
                    .is_some_and(|updated_at| field("updatedAt") == Some(updated_at))
        }
        None => entry.is_none(),
    })
}

fn roll_back(stage: &Path, target: &Path, home: &Path) -> io::Result<bool> {
    if committed(stage, home)? {
        return Ok(false);
    }
    let old = stage.join("old");
    if present(&stage.join("swapped")) && !present(&stage.join("new")) && present(target) {
        let failed = stage.join("failed");
        fs::rename(target, &failed).map_err(at(target))?;
    }
    if !present(&old) {
        return Ok(false);
    }
    if present(target) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists again", target.display()),
        ));
    }
    fs::rename(&old, target).map_err(at(target))?;
    Ok(true)
}

fn discard_stage(stage: &Path) {
    if let Err(error) = remove_tree(stage) {
        eprintln!(
            "vorra: warning: staging {} was not removed ({error}); remove it by hand",
            shown(stage.display())
        );
    }
}

fn remove_tree(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {}
    }
    make_writable(path)?;
    fs::remove_dir_all(path).map_err(at(path))
}

fn make_writable(dir: &Path) -> io::Result<()> {
    if !fs::symlink_metadata(dir).map_err(at(dir))?.is_dir() {
        return Ok(());
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(at(dir))?;
    for entry in fs::read_dir(dir).map_err(at(dir))? {
        let entry = entry.map_err(at(dir))?;
        if entry.file_type().map_err(at(&entry.path()))?.is_dir() {
            make_writable(&entry.path())?;
        }
    }
    Ok(())
}

fn valid_folder(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && !name.starts_with('.') && !name.contains(['/', '\0'])
}

fn target_name(stage: &Path) -> Option<String> {
    let mut bytes = Vec::new();
    File::open(stage.join("target"))
        .ok()?
        .take(512)
        .read_to_end(&mut bytes)
        .ok()?;
    let name = String::from_utf8(bytes).ok()?;
    valid_folder(&name).then_some(name)
}

#[derive(Default)]
struct Budget {
    entries: usize,
    bytes: u64,
}

impl Budget {
    fn charge(&mut self, bytes: u64, path: &Path) -> Result<(), SkillsCliError> {
        self.entries += 1;
        self.bytes = self.bytes.saturating_add(bytes);
        if self.entries > MAX_COPY_ENTRIES || self.bytes > MAX_COPY_BYTES {
            return Err(SkillsCliError::Failed(format!(
                "skill at {} is larger than {MAX_COPY_ENTRIES} entries or 64 MiB",
                shown(path.display())
            )));
        }
        Ok(())
    }
}

fn sorted_entries(dir: &Path) -> io::Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(dir)
        .and_then(|entries| entries.collect::<Result<Vec<_>, _>>())
        .map_err(at(dir))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn named(entry: &fs::DirEntry, names: &[&str]) -> bool {
    entry
        .file_name()
        .to_str()
        .is_some_and(|name| names.contains(&name))
}

fn copy_tree(
    source: &Path,
    destination: &Path,
    budget: &mut Budget,
    depth: usize,
) -> Result<(), SkillsCliError> {
    if depth > MAX_COPY_DEPTH {
        return Err(SkillsCliError::Failed(format!(
            "{} is nested deeper than {MAX_COPY_DEPTH} levels",
            shown(source.display())
        )));
    }
    fs::DirBuilder::new()
        .mode(0o755)
        .create(destination)
        .map_err(at(destination))?;
    for entry in sorted_entries(source)? {
        let path = entry.path();
        let kind = entry.file_type()?;
        let output = destination.join(entry.file_name());
        if kind.is_symlink() {
            return Err(SkillsCliError::Failed(format!(
                "{} is a symlink; vorra skills add never copies or follows symlinks",
                shown(path.display())
            )));
        } else if kind.is_dir() {
            if !named(&entry, &COPY_EXCLUDED_DIRS) {
                budget.charge(0, &path)?;
                copy_tree(&path, &output, budget, depth + 1)?;
            }
        } else if kind.is_file() {
            if !named(&entry, &["metadata.json"]) {
                let metadata = entry.metadata()?;
                budget.charge(metadata.len(), &path)?;
                fs::copy(&path, &output).map_err(at(&path))?;
                fs::set_permissions(&output, fs::Permissions::from_mode(metadata.mode() & 0o755))
                    .map_err(at(&output))?;
            }
        } else {
            return Err(SkillsCliError::Failed(format!(
                "{} is not a regular file or directory",
                shown(path.display())
            )));
        }
    }
    Ok(())
}

fn folder_hash(dir: &Path) -> Result<String, SkillsCliError> {
    fn collect(
        base: &Path,
        dir: &Path,
        files: &mut Vec<(String, PathBuf)>,
        budget: &mut Budget,
        depth: usize,
    ) -> Result<(), SkillsCliError> {
        if depth > MAX_COPY_DEPTH {
            return Err(SkillsCliError::Failed(format!(
                "{} is nested too deeply",
                shown(dir.display())
            )));
        }
        for entry in sorted_entries(dir)? {
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_dir() && !named(&entry, &HASH_EXCLUDED_DIRS) {
                collect(base, &path, files, budget, depth + 1)?;
            } else if kind.is_file() {
                budget.charge(entry.metadata()?.len(), &path)?;
                let relative = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                files.push((relative, path));
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    collect(dir, dir, &mut files, &mut Budget::default(), 0)?;
    files.sort_by_cached_key(|(relative, _)| collation_key(relative));
    let mut hasher = Sha256::new();
    for (relative, path) in files {
        hasher.update(relative.as_bytes());
        hasher.update(fs::read(&path).map_err(at(&path))?);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn collation_key(path: &str) -> (Vec<u32>, Vec<bool>) {
    const ORDER: &str = " _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$0123456789abcdefghijklmnopqrstuvwxyz";
    path.chars()
        .map(|character| {
            let primary = ORDER
                .find(character.to_ascii_lowercase())
                .map_or(0x100 + u32::from(character), |index| index as u32);
            (primary, character.is_ascii_uppercase())
        })
        .unzip()
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn skills_root(home: &Path) -> PathBuf {
    home.join(".agents").join("skills")
}

pub(super) fn add(
    home: &Path,
    arguments: &AddArguments,
    cancellation: &CancellationToken,
) -> Result<(), SkillsCliError> {
    let mut source = fetch::parse_source(&arguments.source)?;
    if let Some(wanted) = &arguments.reference {
        match &mut source {
            Source::Git { reference, .. } if fetch::valid_ref(wanted) => {
                *reference = Some(wanted.clone());
            }
            Source::Git { .. } => {
                return Err(SkillsCliError::Failed(format!(
                    "--ref {} is not a valid branch or tag name",
                    shown(wanted)
                )));
            }
            Source::Local(_) => {
                return Err(SkillsCliError::Failed(
                    "--ref applies only to git sources".to_owned(),
                ));
            }
        }
    }
    let mut fallback = format!("npx skills add {} -g -a codex -y", arguments.source);
    for skill in &arguments.skills {
        fallback.push_str(&format!(" --skill {skill}"));
    }
    let staging = Staging::open(home)?;
    let (checkout, base, subpath) = match &source {
        Source::Git {
            url,
            reference,
            subpath,
            ..
        } => {
            let checkout = fetch::clone(url, reference.as_deref(), &fallback, cancellation)?;
            let base = checkout.repo.clone();
            (Some(checkout), base, subpath.clone())
        }
        Source::Local(path) => {
            if !fs::metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
                return Err(SkillsCliError::Failed(format!(
                    "{} is not a directory",
                    shown(path.display())
                )));
            }
            (None, path.clone(), None)
        }
    };
    let (found, warnings) = fetch::find_skills(&base, subpath.as_deref(), cancellation)?;
    for warning in warnings {
        eprintln!("vorra: warning: {warning}");
    }
    let filter = match &source {
        Source::Git { skill, .. } => skill.as_deref(),
        Source::Local(_) => None,
    };
    let selected = select(&found, arguments, filter)?;
    let (lock_source, source_type, source_url) = source.lock_identity();
    let reference = match &source {
        Source::Git { reference, .. } => reference.clone(),
        Source::Local(_) => None,
    };
    let existing = lock::read_global_raw(home)?;
    let mut targets = BTreeSet::new();
    for skill in &selected {
        let dir_name = skill.name.dir_name();
        if !targets.insert(dir_name.clone()) {
            return Err(SkillsCliError::Failed(format!(
                "two selected skills install to the same directory {dir_name}"
            )));
        }
        let tracked = existing
            .get(skill.name.as_str())
            .and_then(|entry| entry.get("sourceUrl"))
            .and_then(Value::as_str)
            == Some(source_url.as_str());
        if !arguments.yes && !tracked && present(&skills_root(home).join(dir_name.as_str())) {
            return Err(SkillsCliError::Failed(format!(
                "{} already exists and was not installed from {}; pass --yes to replace it",
                skills_root(home).join(dir_name.as_str()).display(),
                shown(&source_url)
            )));
        }
    }
    for skill in selected {
        if cancellation.is_cancelled() {
            return Err(SkillsCliError::Cancelled);
        }
        let (hash, skill_path) = match &checkout {
            Some(checkout) => (
                fetch::tree_hash(checkout, &skill.folder, &fallback, cancellation)?,
                Some(if skill.folder.is_empty() {
                    "SKILL.md".to_owned()
                } else {
                    format!("{}/SKILL.md", skill.folder)
                }),
            ),
            None => (folder_hash(&skill.dir)?, None),
        };
        let key = skill.name.as_str().to_owned();
        let now = timestamp();
        let expected = Expected::Hash {
            key: &key,
            hash: &hash,
            updated_at: &now,
        };
        let target = staging.replace(
            skill.name.dir_name().as_str(),
            Some(&skill.dir),
            &expected,
            || {
                lock::write_global(home, |skills| {
                    let installed_at = skills
                        .get(&key)
                        .and_then(|entry| entry.get("installedAt"))
                        .cloned()
                        .unwrap_or_else(|| json!(now));
                    let mut entry = json!({
                        "source": lock_source,
                        "sourceType": source_type,
                        "sourceUrl": source_url,
                        "skillFolderHash": hash,
                        "installedAt": installed_at,
                        "updatedAt": now,
                    });
                    if let Some(reference) = &reference {
                        entry["ref"] = json!(reference);
                    }
                    if let Some(skill_path) = &skill_path {
                        entry["skillPath"] = json!(skill_path);
                    }
                    skills.insert(key.clone(), entry);
                })
                .map_err(SkillsCliError::from)
            },
        )?;
        println!(
            "installed {} -> {} ({source_type} {}, {hash})",
            shown(&key),
            shown(target.display()),
            shown(&lock_source)
        );
    }
    Ok(())
}

fn select<'a>(
    found: &'a [FoundSkill],
    arguments: &AddArguments,
    filter: Option<&str>,
) -> Result<Vec<&'a FoundSkill>, SkillsCliError> {
    if found.is_empty() {
        return Err(SkillsCliError::Failed(format!(
            "no SKILL.md with a name and a description was found in {}",
            shown(&arguments.source)
        )));
    }
    let wanted: Vec<&str> = arguments
        .skills
        .iter()
        .map(String::as_str)
        .chain(filter)
        .collect();
    let names = || {
        found
            .iter()
            .map(|skill| shown(&skill.name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if arguments.all {
        if !wanted.is_empty() {
            return Err(SkillsCliError::Failed(
                "--all cannot be combined with a skill name".to_owned(),
            ));
        }
        return Ok(found.iter().collect());
    }
    if wanted.is_empty() {
        return if found.len() == 1 {
            Ok(found.iter().collect())
        } else {
            Err(SkillsCliError::Failed(format!(
                "{} contains {} skills ({}); pass --skill NAME or --all",
                shown(&arguments.source),
                found.len(),
                names()
            )))
        };
    }
    let mut selected: Vec<&FoundSkill> = Vec::new();
    for name in wanted {
        let lowered = name.to_lowercase();
        let skill = found
            .iter()
            .find(|skill| skill.name.as_str().to_lowercase() == lowered)
            .ok_or_else(|| {
                SkillsCliError::Failed(format!(
                    "no skill named {} in {}; available: {}",
                    shown(name),
                    shown(&arguments.source),
                    names()
                ))
            })?;
        if !selected.iter().any(|chosen| chosen.name == skill.name) {
            selected.push(skill);
        }
    }
    Ok(selected)
}

fn skill_folder(skill_path: &str) -> Option<&str> {
    let folder = skill_path.strip_suffix("SKILL.md")?;
    let folder = folder.strip_suffix('/').unwrap_or(folder);
    let safe = folder.is_empty()
        || (!folder.starts_with('/')
            && folder
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."));
    (safe && (folder.is_empty() || skill_path.len() == folder.len() + "/SKILL.md".len()))
        .then_some(folder)
}

pub(super) fn update(
    home: &Path,
    extra_roots: &[PathBuf],
    arguments: &UpdateArguments,
    cancellation: &CancellationToken,
) -> Result<(), SkillsCliError> {
    let staging = if arguments.check {
        recover_if_idle(home);
        None
    } else {
        Some(Staging::open(home)?)
    };
    let raw = lock::read_global_raw(home)?;
    let mut entries = BTreeMap::new();
    for (key, value) in &raw {
        match serde_json::from_value::<LockEntry>(value.clone()) {
            Ok(entry) => {
                entries.insert(key.clone(), entry);
            }
            Err(error) => eprintln!(
                "vorra: warning: lock entry {} is malformed ({error}); skipped",
                shown(key)
            ),
        }
    }
    if !arguments.names.is_empty() {
        let wanted: BTreeSet<SkillDirName> = arguments
            .names
            .iter()
            .map(|name| SkillDirName::sanitize(name))
            .collect();
        for name in &arguments.names {
            let sanitized = SkillDirName::sanitize(name);
            if !entries
                .keys()
                .any(|key| SkillDirName::sanitize(key) == sanitized)
            {
                return Err(SkillsCliError::NotFound(name.clone()));
            }
        }
        entries.retain(|key, _| wanted.contains(&SkillDirName::sanitize(key)));
    }
    if entries.is_empty() {
        println!("No global skills are tracked in the lock.");
        return Ok(());
    }
    let mut groups: BTreeMap<(String, Option<String>), Vec<Checkable>> = BTreeMap::new();
    let (mut skipped, mut current, mut changed, mut failed) = (0, 0, 0, 0);
    let agent_dirs = agent_dirs(home, extra_roots);
    for (key, entry) in entries {
        let dir_name = SkillDirName::sanitize(&key);
        let reason = if !matches!(entry.source_type.as_str(), "github" | "git") {
            Some(format!(
                "{} source; Vorra fetches only github and git sources",
                entry.source_type
            ))
        } else if entry.source_url.is_none() {
            Some("no sourceUrl recorded".to_owned())
        } else if entry
            .skill_folder_hash
            .as_ref()
            .is_none_or(String::is_empty)
        {
            Some("no skillFolderHash recorded".to_owned())
        } else if entry.skill_path.as_deref().and_then(skill_folder).is_none() {
            Some("no usable skillPath recorded".to_owned())
        } else if !present(&skills_root(home).join(dir_name.as_str())) {
            match agent_dirs
                .iter()
                .find(|dir| present(&dir.join(dir_name.as_str())))
            {
                Some(dir) => Some(format!(
                    "agent-only npx install in {}, not managed by Vorra; run {NPX_UPDATE} {}",
                    dir.display(),
                    dir_name.as_str()
                )),
                None => {
                    Some("not installed on disk; reinstall it with vorra skills add".to_owned())
                }
            }
        } else if entry
            .reference
            .as_deref()
            .is_some_and(|reference| !fetch::valid_ref(reference))
        {
            Some("ref is not a branch or tag name".to_owned())
        } else {
            None
        };
        if let Some(reason) = reason {
            println!("skipped {}: {}", shown(&key), shown(reason));
            skipped += 1;
            continue;
        }
        let url = entry.source_url.clone().unwrap_or_default();
        if !url.starts_with("https://") && !url.starts_with("ssh://") && !url.starts_with("git@") {
            println!(
                "skipped {}: source URL {} is not https or ssh",
                shown(&key),
                shown(&url)
            );
            skipped += 1;
            continue;
        }
        let folder = entry
            .skill_path
            .as_deref()
            .and_then(skill_folder)
            .unwrap_or_default()
            .to_owned();
        groups
            .entry((url, entry.reference.clone()))
            .or_default()
            .push((key, entry, folder));
    }
    for ((url, reference), items) in groups {
        if cancellation.is_cancelled() {
            return Err(SkillsCliError::Cancelled);
        }
        println!("checking {}", shown(&url));
        let checkout = match fetch::clone(&url, reference.as_deref(), NPX_UPDATE, cancellation) {
            Ok(checkout) => checkout,
            Err(error @ (SkillsCliError::Cancelled | SkillsCliError::GitMissing(_))) => {
                return Err(error);
            }
            Err(error) => {
                println!("failed to check {}: {}", shown(&url), shown(error));
                failed += items.len();
                continue;
            }
        };
        for (key, entry, folder) in items {
            match update_one(
                home,
                staging.as_ref(),
                &checkout,
                &key,
                &entry,
                &folder,
                cancellation,
            ) {
                Ok(Outcome::Current) => {
                    println!("up to date {}", shown(&key));
                    current += 1;
                }
                Ok(Outcome::Available(hash)) => {
                    println!("update available {} ({hash})", shown(&key));
                    changed += 1;
                }
                Ok(Outcome::Updated(hash)) => {
                    println!("updated {} ({hash})", shown(&key));
                    changed += 1;
                }
                Ok(Outcome::Deleted) => {
                    println!(
                        "skipped {}: {} no longer exists upstream; kept",
                        shown(&key),
                        shown(entry.skill_path.as_deref().unwrap_or_default())
                    );
                    skipped += 1;
                }
                Err(error @ (SkillsCliError::Cancelled | SkillsCliError::GitMissing(_))) => {
                    return Err(error);
                }
                Err(error) => {
                    println!("failed {}: {}", shown(&key), shown(error));
                    failed += 1;
                }
            }
        }
    }
    let verb = if arguments.check {
        "with updates"
    } else {
        "updated"
    };
    println!("{changed} {verb}, {current} up to date, {skipped} skipped, {failed} failed");
    if failed > 0 {
        return Err(SkillsCliError::Failed(format!(
            "{failed} skill(s) could not be checked or updated"
        )));
    }
    Ok(())
}

type Checkable = (String, LockEntry, String);

enum Outcome {
    Current,
    Available(String),
    Updated(String),
    Deleted,
}

fn update_one(
    home: &Path,
    staging: Option<&Staging>,
    checkout: &Checkout,
    key: &str,
    entry: &LockEntry,
    folder: &str,
    cancellation: &CancellationToken,
) -> Result<Outcome, SkillsCliError> {
    let dir = checkout.repo.join(folder);
    if !present(&dir) {
        return Ok(Outcome::Deleted);
    }
    match read_skill_file(&dir) {
        Ok(Some(_)) => {}
        Ok(None) => return Ok(Outcome::Deleted),
        Err(issue) => {
            return Err(SkillsCliError::Failed(format!(
                "upstream SKILL.md in {} is unusable: {}",
                shown(folder),
                shown(issue)
            )));
        }
    }
    let hash = fetch::tree_hash(checkout, folder, NPX_UPDATE, cancellation)?;
    if Some(&hash) == entry.skill_folder_hash.as_ref() {
        return Ok(Outcome::Current);
    }
    let Some(staging) = staging else {
        return Ok(Outcome::Available(hash));
    };
    let subpath = (!folder.is_empty()).then_some(folder);
    let (found, _) = fetch::find_skills(&checkout.repo, subpath, cancellation)?;
    let skill = found.iter().find(|skill| skill.dir == dir).ok_or_else(|| {
        SkillsCliError::Failed(format!("upstream SKILL.md in {folder} is no longer valid"))
    })?;
    if skill.name.as_str() != key {
        return Err(SkillsCliError::Failed(format!(
            "upstream skill is now named {}; reinstall it with vorra skills add",
            shown(&skill.name)
        )));
    }
    let now = timestamp();
    let expected = Expected::Hash {
        key,
        hash: &hash,
        updated_at: &now,
    };
    staging.replace(
        skill.name.dir_name().as_str(),
        Some(&skill.dir),
        &expected,
        || {
            lock::write_global(home, |skills| {
                if let Some(Value::Object(entry)) = skills.get_mut(key) {
                    entry.insert("skillFolderHash".to_owned(), json!(hash));
                    entry.insert("updatedAt".to_owned(), json!(now));
                }
            })
            .map_err(SkillsCliError::from)
        },
    )?;
    Ok(Outcome::Updated(hash))
}

fn agent_dirs(home: &Path, extra: &[PathBuf]) -> Vec<PathBuf> {
    let variable = |name: &str| {
        std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    let config = variable("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
    let claude = variable("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home.join(".claude"));
    let mut dirs = vec![
        claude.join("skills"),
        config.join("agents/skills"),
        config.join("goose/skills"),
        config.join("opencode/skills"),
        home.join(".cursor/skills"),
        home.join(".gemini/skills"),
        home.join(".codeium/windsurf/skills"),
        home.join(".copilot/skills"),
        home.join(".factory/skills"),
        home.join(".kiro/skills"),
        home.join(".roo/skills"),
        home.join(".trae/skills"),
    ];
    for dir in extra {
        if !dirs.contains(dir) {
            dirs.push(dir.clone());
        }
    }
    dirs
}

pub(super) fn remove(
    home: &Path,
    catalog: &Catalog,
    extra_roots: &[PathBuf],
    arguments: &RemoveArguments,
) -> Result<(), SkillsCliError> {
    let staging = Staging::open(home)?;
    let raw = lock::read_global_raw(home)?;
    let root = skills_root(home);
    let mut identities: BTreeMap<SkillDirName, (String, Option<String>)> = BTreeMap::new();
    for entry in fs::read_dir(&root).map_err(at(&root))? {
        let entry = entry.map_err(at(&root))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with('.')
            && entry.file_type().map_err(at(&entry.path()))?.is_dir()
            && present(&entry.path().join("SKILL.md"))
        {
            identities.insert(SkillDirName::sanitize(&name), (name.clone(), Some(name)));
        }
    }
    for key in raw.keys() {
        identities
            .entry(SkillDirName::sanitize(key))
            .or_insert_with(|| (key.clone(), None))
            .0 = key.clone();
    }
    let mut matched: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut missing = Vec::new();
    for name in &arguments.names {
        let sanitized = SkillDirName::sanitize(name);
        if let Some((identity, folder)) = identities.get(&sanitized) {
            matched.insert(identity.clone(), folder.clone());
            continue;
        }
        let by_source: Vec<_> = raw
            .iter()
            .filter(|(_, entry)| entry.get("source").and_then(Value::as_str) == Some(name))
            .map(|(key, _)| {
                let folder = identities
                    .get(&SkillDirName::sanitize(key))
                    .and_then(|(_, folder)| folder.clone());
                (key.clone(), folder)
            })
            .collect();
        if !by_source.is_empty() {
            matched.extend(by_source);
            continue;
        }
        let read_only = catalog
            .skills
            .iter()
            .find(|record| {
                !matches!(record.root, SkillRoot::Global(_))
                    && (record.name.dir_name() == sanitized || record.dir_name == sanitized)
            })
            .map(|record| record.root.clone());
        match read_only {
            Some(SkillRoot::Project(path)) => {
                return Err(SkillsCliError::ReadOnly {
                    name: name.clone(),
                    root: path,
                    command: format!("npx skills remove {name}"),
                });
            }
            Some(root) => {
                return Err(SkillsCliError::ReadOnly {
                    name: name.clone(),
                    root: root.path().clone(),
                    command: format!("npx skills remove {name} -g"),
                });
            }
            None => missing.push(name.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(SkillsCliError::NotFound(missing.join(", ")));
    }
    if !arguments.yes {
        let listed = matched.keys().map(shown).collect::<Vec<_>>().join(", ");
        if !io::stdin().is_terminal() {
            return Err(SkillsCliError::Failed(format!(
                "would remove {listed}; pass --yes to confirm without a terminal"
            )));
        }
        print!("Remove {listed}? [y/N] ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().lock().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            return Err(SkillsCliError::Failed("nothing was removed".to_owned()));
        }
    }
    let agent_dirs = agent_dirs(home, extra_roots);
    for (identity, folder) in matched {
        let dir_name = SkillDirName::sanitize(&identity);
        let folder = folder.unwrap_or_else(|| dir_name.as_str().to_owned());
        let target = root.join(&folder);
        let links: Vec<PathBuf> = match target.canonicalize() {
            Ok(canonical) => agent_dirs
                .iter()
                .map(|dir| dir.join(dir_name.as_str()))
                .filter(|link| {
                    fs::symlink_metadata(link).is_ok_and(|metadata| metadata.is_symlink())
                        && link
                            .canonicalize()
                            .is_ok_and(|resolved| resolved == canonical)
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        let tracked = raw.contains_key(&identity);
        staging.replace(&folder, None, &Expected::Absent { key: &identity }, || {
            if tracked {
                lock::write_global(home, |skills| {
                    skills.remove(&identity);
                })?;
            }
            Ok(())
        })?;
        for link in links {
            match fs::remove_file(&link) {
                Ok(()) => println!("removed link {}", shown(link.display())),
                Err(error) => eprintln!(
                    "vorra: warning: link {} was not removed: {error}",
                    shown(link.display())
                ),
            }
        }
        println!(
            "removed {}{}",
            shown(&identity),
            if tracked { " and its lock entry" } else { "" }
        );
    }
    Ok(())
}

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use rustix::fs::{FileType, Mode, OFlags};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use super::frontmatter::{self, Document};
use super::lock::{self, Lock, LockEntry};
use super::{
    MAX_COMPATIBILITY_BYTES, MAX_DESCRIPTION_BYTES, MAX_SKILL_DIRS_PER_ROOT, MAX_SKILL_FILE_BYTES,
    SkillDirName, SkillIssue, SkillName, SkillRecord, SkillRoot,
};

pub const MAX_ROOT_ENTRIES: usize = 4096;
const PREVIEW_LINES: usize = 12;
const PREVIEW_BYTES: usize = 1024;

#[derive(Clone, Debug)]
pub struct SkillRoots {
    pub home: PathBuf,
    pub workspace: Option<PathBuf>,
    pub extra: Vec<PathBuf>,
}

impl SkillRoots {
    pub fn new(home: PathBuf, workspace: Option<PathBuf>, extra: Option<Vec<PathBuf>>) -> Self {
        let extra = extra.unwrap_or_else(|| vec![default_extra_root(&home)]);
        Self {
            home,
            workspace,
            extra,
        }
    }

    fn global_root(&self) -> PathBuf {
        self.home.join(".agents").join("skills")
    }

    pub(super) fn link_targets(&self) -> Vec<PathBuf> {
        self.workspace
            .iter()
            .cloned()
            .chain([self.global_root()])
            .chain(self.extra.iter().cloned())
            .filter_map(|path| path.canonicalize().ok())
            .collect()
    }

    fn roots(&self) -> Vec<SkillRoot> {
        let identity = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_owned());
        let global = SkillRoot::Global(self.global_root());
        let mut seen = BTreeSet::from([identity(global.path())]);
        let mut roots = Vec::new();
        if let Some(workspace) = &self.workspace {
            let project = workspace.join(".agents").join("skills");
            if seen.insert(identity(&project)) {
                roots.push(SkillRoot::Project(project));
            }
        }
        roots.push(global);
        for extra in &self.extra {
            if seen.insert(identity(extra)) {
                roots.push(SkillRoot::Extra(extra.clone()));
            }
        }
        roots
    }
}

fn default_extra_root(home: &Path) -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map_or_else(|| home.join(".claude"), PathBuf::from)
        .join("skills")
}

#[derive(Clone, Debug, Serialize)]
pub struct RejectedSkill {
    pub label: String,
    pub root: SkillRoot,
    pub dir: PathBuf,
    pub issues: Vec<SkillIssue>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MissingSkill {
    pub name: String,
    pub scope: &'static str,
    pub entry: LockEntry,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Catalog {
    pub skills: Vec<SkillRecord>,
    pub rejected: Vec<RejectedSkill>,
    pub missing: Vec<MissingSkill>,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub struct Cancelled;

enum Scanned {
    Skill(Box<SkillRecord>),
    Rejected(RejectedSkill),
}

pub fn discover(
    roots: &SkillRoots,
    cancellation: &CancellationToken,
) -> Result<Catalog, Cancelled> {
    let mut catalog = Catalog::default();
    let global_lock = lock::read_global(&roots.home, &mut catalog.warnings);
    let project_lock = roots
        .workspace
        .as_deref()
        .map(|workspace| lock::read_project(workspace, &mut catalog.warnings))
        .unwrap_or_default();
    let mut offered = BTreeSet::new();
    let targets = roots.link_targets();
    let mut project_scanned = false;
    for root in roots.roots() {
        if let SkillRoot::Project(path) = &root {
            project_scanned = trusted_root(path, &mut catalog.warnings);
            if !project_scanned {
                continue;
            }
        }
        for scanned in scan_root(&root, &targets, &mut catalog.warnings, cancellation)? {
            match scanned {
                Scanned::Skill(mut record) => {
                    let lock = match record.root {
                        SkillRoot::Project(_) => &project_lock,
                        SkillRoot::Global(_) | SkillRoot::Extra(_) => &global_lock,
                    };
                    record.source = lock
                        .find(record.name.as_str())
                        .map(|(_, entry)| entry.clone());
                    record.shadowed = !offered.insert(record.name.dir_name());
                    catalog.skills.push(*record);
                }
                Scanned::Rejected(rejected) => catalog.rejected.push(rejected),
            }
        }
    }
    let folder =
        |dir: &Path| SkillDirName::sanitize(&dir.file_name().unwrap_or_default().to_string_lossy());
    let installed = |project: bool| -> BTreeSet<SkillDirName> {
        let in_scope = |root: &SkillRoot| matches!(root, SkillRoot::Project(_)) == project;
        catalog
            .skills
            .iter()
            .filter(|record| in_scope(&record.root))
            .flat_map(|record| [record.name.dir_name(), folder(&record.dir)])
            .chain(
                catalog
                    .rejected
                    .iter()
                    .filter(|rejected| in_scope(&rejected.root))
                    .map(|rejected| folder(&rejected.dir)),
            )
            .collect()
    };
    let installed_project = installed(true);
    let installed_global = installed(false);
    if project_scanned {
        missing(
            &project_lock,
            &installed_project,
            "project",
            &mut catalog.missing,
        );
    }
    missing(
        &global_lock,
        &installed_global,
        "global",
        &mut catalog.missing,
    );
    Ok(catalog)
}

fn missing(
    lock: &Lock,
    installed: &BTreeSet<SkillDirName>,
    scope: &'static str,
    output: &mut Vec<MissingSkill>,
) {
    output.extend(
        lock.entries
            .iter()
            .filter(|(key, _)| !installed.contains(&SkillDirName::sanitize(key)))
            .map(|(key, entry)| MissingSkill {
                name: key.clone(),
                scope,
                entry: entry.clone(),
            }),
    );
}

fn trusted_root(path: &Path, warnings: &mut Vec<String>) -> bool {
    match trusted_directory(path) {
        Ok(()) => true,
        Err(None) => false,
        Err(Some(reason)) => {
            warnings.push(format!(
                "project root {} was not scanned: {reason}",
                path.display()
            ));
            false
        }
    }
}

fn scan_root(
    root: &SkillRoot,
    targets: &[PathBuf],
    warnings: &mut Vec<String>,
    cancellation: &CancellationToken,
) -> Result<Vec<Scanned>, Cancelled> {
    let entries = match std::fs::read_dir(root.path()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            warnings.push(format!(
                "{} root {} could not be read: {error}",
                root.label(),
                root.path().display()
            ));
            return Ok(Vec::new());
        }
    };
    let mut candidates = Vec::new();
    for (visited, entry) in entries.enumerate() {
        if cancellation.is_cancelled() {
            return Err(Cancelled);
        }
        if visited == MAX_ROOT_ENTRIES {
            warnings.push(format!(
                "{} root {} has more than {MAX_ROOT_ENTRIES} entries; the rest were not scanned",
                root.label(),
                root.path().display()
            ));
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.push(format!(
                    "{} root {} could not be listed: {error}",
                    root.label(),
                    root.path().display()
                ));
                break;
            }
        };
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if !kind.is_dir() && !kind.is_symlink() {
            continue;
        }
        if candidates.len() == MAX_SKILL_DIRS_PER_ROOT {
            warnings.push(format!(
                "{} root {} has more than {MAX_SKILL_DIRS_PER_ROOT} directories; the rest were not scanned",
                root.label(),
                root.path().display()
            ));
            break;
        }
        candidates.push(entry.path());
    }
    candidates.sort();
    let mut scanned = Vec::with_capacity(candidates.len());
    for dir in candidates {
        if cancellation.is_cancelled() {
            return Err(Cancelled);
        }
        scanned.extend(scan_dir(root, targets, dir));
    }
    Ok(scanned)
}

pub(super) fn trusted_directory(path: &Path) -> Result<(), Option<String>> {
    let directory = match rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(directory) => directory,
        Err(rustix::io::Errno::NOENT) => return Err(None),
        Err(error) => return Err(Some(io::Error::from(error).to_string())),
    };
    let status = rustix::fs::fstat(&directory).map_err(|error| Some(error.to_string()))?;
    trusted_status(&status).map_err(|reason| Some(reason.to_owned()))
}

fn scan_dir(root: &SkillRoot, targets: &[PathBuf], dir: PathBuf) -> Option<Scanned> {
    let folder = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let rejected = |label: String, issues: Vec<SkillIssue>| {
        Some(Scanned::Rejected(RejectedSkill {
            label,
            root: root.clone(),
            dir: dir.clone(),
            issues,
        }))
    };
    let allowed = matches!(root, SkillRoot::Project(_)).then_some(targets);
    let contents = match read_skill_file_within(&dir, allowed, true) {
        Ok(Some(contents)) => contents,
        Ok(None) => return None,
        Err(issue) => return rejected(folder, vec![issue]),
    };
    let Some(Document { frontmatter, body }) = frontmatter::split(&contents) else {
        return rejected(folder, vec![SkillIssue::MissingFrontmatter]);
    };
    let fields = match frontmatter::parse(frontmatter) {
        Ok(fields) => fields,
        Err(issue) => return rejected(folder, vec![issue]),
    };
    let mut issues = fields.issues;
    let Some(raw_name) = fields.name else {
        if !issues.contains(&SkillIssue::NotAString { field: "name" }) {
            issues.insert(0, SkillIssue::MissingName);
        }
        return rejected(folder, issues);
    };
    let name = match SkillName::parse(&raw_name) {
        Ok(name) => name,
        Err(issue) => {
            issues.insert(0, issue);
            return rejected(raw_name, issues);
        }
    };
    if !name.follows_spec_grammar() {
        issues.push(SkillIssue::NameGrammar);
    }
    if folder != name.as_str() {
        issues.push(SkillIssue::DirectoryMismatch {
            directory: folder.clone(),
        });
    }
    let description = fields.description.unwrap_or_default();
    if description.is_empty() {
        issues.push(SkillIssue::MissingDescription);
    } else if description.len() > MAX_DESCRIPTION_BYTES {
        issues.push(SkillIssue::DescriptionTooLong {
            bytes: description.len(),
        });
    }
    if let Some(compatibility) = &fields.compatibility
        && compatibility.len() > MAX_COMPATIBILITY_BYTES
    {
        issues.push(SkillIssue::CompatibilityTooLong {
            bytes: compatibility.len(),
        });
    }
    Some(Scanned::Skill(Box::new(SkillRecord {
        dir_name: name.dir_name(),
        name,
        description,
        root: root.clone(),
        dir,
        license: fields.license,
        compatibility: fields.compatibility,
        metadata: fields.metadata,
        allowed_tools: fields.allowed_tools,
        source: None,
        issues,
        shadowed: false,
        preview: preview(body),
    })))
}

fn preview(body: &str) -> String {
    let mut preview = String::new();
    for line in body.lines().take(PREVIEW_LINES) {
        if preview.len() + line.len() + 1 > PREVIEW_BYTES {
            break;
        }
        preview.push_str(line);
        preview.push('\n');
    }
    preview
}

pub fn read_body(record: &SkillRecord) -> Result<String, SkillIssue> {
    let contents =
        read_skill_file_within(&record.dir, None, true)?.ok_or_else(|| SkillIssue::Unreadable {
            message: "SKILL.md no longer exists".to_owned(),
        })?;
    let document = frontmatter::split(&contents).ok_or(SkillIssue::MissingFrontmatter)?;
    Ok(document.body.to_owned())
}

pub(super) fn read_skill_file(dir: &Path) -> Result<Option<String>, SkillIssue> {
    read_skill_file_within(dir, None, false)
}

fn read_skill_file_within(
    dir: &Path,
    allowed: Option<&[PathBuf]>,
    trusted: bool,
) -> Result<Option<String>, SkillIssue> {
    let Some(directory) = open_skill_dir(dir, allowed, trusted)? else {
        return Ok(None);
    };
    read_file_at(&directory, "SKILL.md", MAX_SKILL_FILE_BYTES, false, trusted)
        .map(|file| file.map(|(text, _)| text))
}

pub(super) fn read_file_at(
    directory: &OwnedFd,
    name: &str,
    maximum: u64,
    truncate: bool,
    trusted: bool,
) -> Result<Option<(String, Option<u64>)>, SkillIssue> {
    let file = match rustix::fs::openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(file) => file,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP) => return Err(SkillIssue::SkillFileNotRegular),
        Err(error) => return Err(unreadable(error)),
    };
    let status = rustix::fs::fstat(&file).map_err(unreadable)?;
    if FileType::from_raw_mode(status.st_mode) != FileType::RegularFile {
        return Err(SkillIssue::SkillFileNotRegular);
    }
    if trusted && let Err(reason) = trusted_status(&status) {
        return Err(SkillIssue::Untrusted { reason });
    }
    let size = u64::try_from(status.st_size).unwrap_or(u64::MAX);
    if size > maximum && !truncate {
        return Err(SkillIssue::SkillFileTooLarge { bytes: size });
    }
    let mut bytes = Vec::new();
    File::from(file)
        .take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| SkillIssue::Unreadable {
            message: error.to_string(),
        })?;
    let mut truncated = None;
    if bytes.len() as u64 > maximum {
        if !truncate {
            return Err(SkillIssue::SkillFileTooLarge {
                bytes: bytes.len() as u64,
            });
        }
        bytes.truncate(maximum as usize);
        truncated = Some(size.max(maximum + 1));
    }
    match String::from_utf8(bytes) {
        Ok(text) => Ok(Some((text, truncated))),
        Err(error) if truncated.is_some() && error.utf8_error().error_len().is_none() => {
            let valid = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(valid);
            Ok(Some((
                String::from_utf8(bytes).map_err(|_| SkillIssue::NotUtf8)?,
                truncated,
            )))
        }
        Err(_) => Err(SkillIssue::NotUtf8),
    }
}

pub(super) fn trusted_status(status: &rustix::fs::Stat) -> Result<(), &'static str> {
    if status.st_uid != rustix::process::geteuid().as_raw() {
        return Err("not owned by the current user");
    }
    if status.st_mode & 0o002 != 0 {
        return Err("world-writable");
    }
    if status.st_mode & 0o020 != 0 && !private_group(status.st_gid) {
        return Err("group-writable by a group other than the user's private group");
    }
    Ok(())
}

fn private_group(gid: u32) -> bool {
    static PRIVATE: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *PRIVATE.get_or_init(user_private_gid) == Some(gid)
}

fn user_private_gid() -> Option<u32> {
    const LIMIT: u64 = 4 * 1024 * 1024;
    let bounded = |path: &str| {
        let mut text = String::new();
        File::open(path)
            .ok()?
            .take(LIMIT + 1)
            .read_to_string(&mut text)
            .ok()?;
        (text.len() as u64 <= LIMIT).then_some(text)
    };
    let uid = rustix::process::geteuid().as_raw();
    let passwd = bounded("/etc/passwd")?;
    let entries: Vec<Vec<&str>> = passwd
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .map(|line| line.split(':').collect())
        .collect();
    let mut own = entries
        .iter()
        .filter(|fields| fields.len() >= 4 && fields[2].parse::<u32>().ok() == Some(uid));
    let user = own.next()?;
    if own.next().is_some() {
        return None;
    }
    let name = user[0];
    let gid = user[3].parse::<u32>().ok()?;
    if entries
        .iter()
        .filter(|fields| fields.len() < 4 || fields[3].parse::<u32>().ok() == Some(gid))
        .count()
        != 1
    {
        return None;
    }
    let groups = bounded("/etc/group")?;
    let mut matching = groups
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .map(|line| line.split(':').collect::<Vec<&str>>())
        .filter(|fields| fields.len() < 3 || fields[2].parse::<u32>().ok() == Some(gid));
    let group = matching.next()?;
    if matching.next().is_some()
        || group.len() < 4
        || group[0] != name
        || !group[3]
            .split(',')
            .all(|member| member.is_empty() || member == name)
    {
        return None;
    }
    Some(gid)
}

pub(super) fn open_skill_dir(
    dir: &Path,
    allowed: Option<&[PathBuf]>,
    trusted: bool,
) -> Result<Option<OwnedFd>, SkillIssue> {
    let link = std::fs::symlink_metadata(dir)
        .map_err(|error| SkillIssue::Unreadable {
            message: error.to_string(),
        })?
        .file_type()
        .is_symlink();
    let mut flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    if !link {
        flags |= OFlags::NOFOLLOW;
    }
    if link
        && let Some(allowed) = allowed
        && !dir
            .canonicalize()
            .is_ok_and(|target| allowed.iter().any(|root| target.starts_with(root)))
    {
        return Err(SkillIssue::SymlinkRefused {
            reason: "target is outside the project and the known skill roots",
        });
    }
    let directory = match rustix::fs::open(dir, flags, Mode::empty()) {
        Ok(directory) => directory,
        Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR) if link => return Ok(None),
        Err(error) => return Err(unreadable(error)),
    };
    let status = rustix::fs::fstat(&directory).map_err(unreadable)?;
    if (trusted || link)
        && let Err(reason) = trusted_status(&status)
    {
        return Err(if link {
            SkillIssue::SymlinkRefused { reason }
        } else {
            SkillIssue::Untrusted { reason }
        });
    }
    Ok(Some(directory))
}

fn unreadable(error: rustix::io::Errno) -> SkillIssue {
    SkillIssue::Unreadable {
        message: io::Error::from(error).to_string(),
    }
}

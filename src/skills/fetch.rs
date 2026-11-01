use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use super::cli::{SkillsCliError, shown};
use super::discover::read_skill_file;
use super::lock::at;
use super::{SkillIssue, SkillName, frontmatter};

const GIT_TIMEOUT: Duration = Duration::from_secs(300);
const POLL: Duration = Duration::from_millis(50);
const MAX_GIT_OUTPUT: u64 = 4096;
const MAX_TREE_LISTING: u64 = 16 * 1024 * 1024;
const MAX_SCANNED_DIRS: usize = 4096;
const MAX_SCAN_DEPTH: usize = 5;
const SKIP_DIRS: [&str; 5] = ["node_modules", ".git", "dist", "build", "__pycache__"];

#[derive(Clone, Debug)]
pub(super) enum Source {
    Git {
        url: String,
        github: Option<String>,
        reference: Option<String>,
        subpath: Option<String>,
        skill: Option<String>,
    },
    Local(PathBuf),
}

impl Source {
    pub fn lock_identity(&self) -> (String, &'static str, String) {
        match self {
            Self::Git {
                url,
                github: Some(owner_repo),
                ..
            } => (owner_repo.clone(), "github", url.clone()),
            Self::Git { url, .. } => (url.clone(), "git", url.clone()),
            Self::Local(path) => {
                let path = path.to_string_lossy().into_owned();
                (path.clone(), "local", path)
            }
        }
    }
}

pub(super) fn parse_source(input: &str) -> Result<Source, SkillsCliError> {
    let unsupported = |reason: &'static str| SkillsCliError::UnsupportedSource {
        input: input.to_owned(),
        reason,
    };
    if input.is_empty()
        || input.starts_with('-')
        || input.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(unsupported("empty, starts with -, or contains whitespace"));
    }
    if input.contains('#') {
        return Err(unsupported("# fragments are not supported; use --ref"));
    }
    if input.starts_with('/') {
        return Ok(Source::Local(PathBuf::from(input)));
    }
    if let Some(rest) = input.strip_prefix("github:") {
        return shorthand(rest).ok_or_else(|| unsupported("not github:owner/repo[/path]"));
    }
    if let Some(rest) = input
        .strip_prefix("https://github.com/")
        .or_else(|| input.strip_prefix("https://www.github.com/"))
    {
        return github_url(rest).ok_or_else(|| {
            unsupported("only github.com/owner/repo and /tree/REF/PATH URLs are fetched")
        });
    }
    if input.starts_with("https://") || input.starts_with("ssh://") {
        return if input.ends_with(".git") {
            Ok(plain_git(input))
        } else {
            Err(unsupported(
                "well-known and archive URLs are not fetched; git URLs must end in .git",
            ))
        };
    }
    if let Some(rest) = input.strip_prefix("git@") {
        return match rest.split_once(':') {
            Some((host, path)) if !host.is_empty() && !path.is_empty() => Ok(plain_git(input)),
            _ => Err(unsupported("not git@host:path")),
        };
    }
    if input.contains(':') || input.starts_with('.') {
        return Err(unsupported(
            "relative paths and other hosts are not supported natively",
        ));
    }
    shorthand(input).ok_or_else(|| unsupported("not owner/repo[/path] or owner/repo@skill"))
}

fn plain_git(url: &str) -> Source {
    Source::Git {
        url: url.to_owned(),
        github: None,
        reference: None,
        subpath: None,
        skill: None,
    }
}

fn github(
    owner: &str,
    repo: &str,
    reference: Option<&str>,
    subpath: Option<String>,
    skill: Option<&str>,
) -> Option<Source> {
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    if !name_segment(owner) || !name_segment(repo) {
        return None;
    }
    if let Some(reference) = reference {
        valid_ref(reference).then_some(())?;
    }
    Some(Source::Git {
        url: format!("https://github.com/{owner}/{repo}.git"),
        github: Some(format!("{owner}/{repo}")),
        reference: reference.map(str::to_owned),
        subpath,
        skill: skill.map(str::to_owned),
    })
}

fn shorthand(input: &str) -> Option<Source> {
    if let Some((repository, skill)) = input.split_once('@') {
        let (owner, repo) = repository.split_once('/')?;
        if repo.contains('/') || skill.is_empty() {
            return None;
        }
        return github(owner, repo, None, None, Some(skill));
    }
    let mut parts = input.trim_end_matches('/').splitn(3, '/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    let subpath = match parts.next() {
        Some(path) => Some(subpath(path)?),
        None => None,
    };
    github(owner, repo, None, subpath, None)
}

fn github_url(rest: &str) -> Option<Source> {
    let mut parts = rest.trim_end_matches('/').splitn(5, '/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    match (parts.next(), parts.next(), parts.next()) {
        (None, _, _) => github(owner, repo, None, None, None),
        (Some("tree"), Some(reference), path) => github(
            owner,
            repo,
            Some(reference),
            match path {
                Some(path) => Some(subpath(path)?),
                None => None,
            },
            None,
        ),
        _ => None,
    }
}

fn name_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && !segment.starts_with('-')
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn subpath(path: &str) -> Option<String> {
    let path = path.trim_matches('/');
    path.split('/')
        .all(|segment| {
            !segment.is_empty() && segment != "." && segment != ".." && !segment.contains('\\')
        })
        .then(|| path.to_owned())
}

pub(super) fn valid_ref(reference: &str) -> bool {
    !reference.is_empty()
        && !reference.starts_with('-')
        && !reference.contains("..")
        && !reference
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '~' | '^' | ':' | '\\'))
}

pub(super) struct Checkout {
    root: PathBuf,
    pub repo: PathBuf,
}

impl Drop for Checkout {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = rustix::process::kill_process_group(
                rustix::process::Pid::from_child(&self.0),
                rustix::process::Signal::KILL,
            );
            let _ = self.0.wait();
        }
    }
}

pub(super) fn clone(
    url: &str,
    reference: Option<&str>,
    fallback: &str,
    cancellation: &CancellationToken,
) -> Result<Checkout, SkillsCliError> {
    let root = std::env::temp_dir().join(format!("vorra-skills-{}", uuid::Uuid::new_v4()));
    fs::DirBuilder::new().mode(0o700).create(&root)?;
    let checkout = Checkout {
        repo: root.join("repo"),
        root,
    };
    let mut arguments: Vec<&OsStr> = [
        "clone",
        "--depth",
        "1",
        "--filter=blob:none",
        "--no-tags",
        "--no-checkout",
    ]
    .into_iter()
    .map(OsStr::new)
    .collect();
    if let Some(reference) = reference {
        arguments.extend([OsStr::new("--branch"), OsStr::new(reference)]);
    }
    arguments.extend([OsStr::new("--"), OsStr::new(url), checkout.repo.as_os_str()]);
    git(
        &checkout,
        "clone",
        &arguments,
        None,
        MAX_GIT_OUTPUT,
        fallback,
        cancellation,
    )?;
    let in_repo = |command: &'static str| {
        [
            OsStr::new("-C"),
            checkout.repo.as_os_str(),
            OsStr::new(command),
        ]
    };
    let listing = git(
        &checkout,
        "ls-tree",
        &[
            &in_repo("ls-tree")[..],
            &[
                OsStr::new("-r"),
                OsStr::new("-z"),
                OsStr::new("--name-only"),
                OsStr::new("HEAD"),
            ],
        ]
        .concat(),
        None,
        MAX_TREE_LISTING,
        fallback,
        cancellation,
    )?;
    let folders: BTreeSet<&str> = listing
        .split('\0')
        .filter_map(|path| match path.rsplit_once('/') {
            Some((folder, "SKILL.md")) => Some(folder),
            None if path == "SKILL.md" => Some(""),
            _ => None,
        })
        .filter(|folder| !folder.contains(['\n', '\r']))
        .collect();
    if folders.is_empty() {
        return Ok(checkout);
    }
    if folders.len() > MAX_SCANNED_DIRS {
        return Err(SkillsCliError::Failed(format!(
            "{} has more than {MAX_SCANNED_DIRS} SKILL.md files",
            shown(url)
        )));
    }
    let patterns: String = if folders.contains("") {
        "/*\n".to_owned()
    } else {
        folders
            .iter()
            .map(|folder| format!("/{}/\n", sparse_escape(folder)))
            .collect()
    };
    let patterns_path = checkout.root.join("sparse-patterns");
    fs::write(&patterns_path, patterns).map_err(at(&patterns_path))?;
    git(
        &checkout,
        "sparse-checkout",
        &[
            &in_repo("sparse-checkout")[..],
            &[
                OsStr::new("set"),
                OsStr::new("--no-cone"),
                OsStr::new("--stdin"),
            ],
        ]
        .concat(),
        Some(&patterns_path),
        MAX_GIT_OUTPUT,
        fallback,
        cancellation,
    )?;
    git(
        &checkout,
        "checkout",
        &in_repo("checkout"),
        None,
        MAX_GIT_OUTPUT,
        fallback,
        cancellation,
    )?;
    Ok(checkout)
}

fn sparse_escape(folder: &str) -> String {
    let mut escaped = String::with_capacity(folder.len());
    for character in folder.chars() {
        if matches!(character, '\\' | '*' | '?' | '[' | ']' | '!' | '#' | ' ') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

pub(super) fn tree_hash(
    checkout: &Checkout,
    folder: &str,
    fallback: &str,
    cancellation: &CancellationToken,
) -> Result<String, SkillsCliError> {
    let revision = if folder.is_empty() {
        "HEAD^{tree}".to_owned()
    } else {
        format!("HEAD:{folder}")
    };
    let resolve = |revision: &str| {
        git(
            checkout,
            "rev-parse",
            &[
                OsStr::new("-C"),
                checkout.repo.as_os_str(),
                OsStr::new("rev-parse"),
                OsStr::new("--verify"),
                OsStr::new("--end-of-options"),
                OsStr::new(revision),
            ],
            None,
            MAX_GIT_OUTPUT,
            fallback,
            cancellation,
        )
        .map(|output| output.trim().to_ascii_lowercase())
    };
    let hash = resolve(&revision)?;
    if hash.len() != 40 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(SkillsCliError::Git {
            action: "rev-parse",
            message: format!("unexpected output {}", shown(&hash)),
        });
    }
    match resolve(&format!("{hash}^{{tree}}")) {
        Ok(tree) if tree == hash => Ok(hash),
        Err(error @ (SkillsCliError::Cancelled | SkillsCliError::GitMissing(_))) => Err(error),
        _ => Err(SkillsCliError::Failed(format!(
            "{} is not a directory in the repository",
            shown(if folder.is_empty() {
                "the root"
            } else {
                folder
            })
        ))),
    }
}

fn git(
    checkout: &Checkout,
    action: &'static str,
    arguments: &[&OsStr],
    stdin: Option<&Path>,
    limit: u64,
    fallback: &str,
    cancellation: &CancellationToken,
) -> Result<String, SkillsCliError> {
    let stdout_path = checkout.root.join("git-stdout");
    let stderr_path = checkout.root.join("git-stderr");
    let mut command = Command::new("git");
    command
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "credential.interactive=never",
        ])
        .args(arguments)
        .current_dir(&checkout.root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", "https:ssh")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(match stdin {
            Some(path) => Stdio::from(File::open(path).map_err(at(path))?),
            None => Stdio::null(),
        })
        .stdout(File::create(&stdout_path).map_err(at(&stdout_path))?)
        .stderr(File::create(&stderr_path).map_err(at(&stderr_path))?)
        .process_group(0);
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_NAMESPACE",
        "GIT_COMMON_DIR",
    ] {
        command.env_remove(variable);
    }
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        command.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    let mut running = match command.spawn() {
        Ok(child) => Running(child),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(SkillsCliError::GitMissing(fallback.to_owned()));
        }
        Err(error) => return Err(error.into()),
    };
    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        if let Some(status) = running.0.try_wait()? {
            break status;
        }
        if cancellation.is_cancelled() {
            return Err(SkillsCliError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(SkillsCliError::GitTimeout(action));
        }
        std::thread::sleep(POLL);
    };
    if !status.success() {
        let stderr = tail(&stderr_path).map_err(at(&stderr_path))?;
        let message = stderr
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(SkillsCliError::Git {
            action,
            message: shown(if message.is_empty() {
                status.to_string()
            } else {
                message
            }),
        });
    }
    let length = fs::metadata(&stdout_path).map_err(at(&stdout_path))?.len();
    if length > limit {
        return Err(SkillsCliError::Failed(format!(
            "git {action} printed more than {limit} bytes"
        )));
    }
    let bytes = fs::read(&stdout_path).map_err(at(&stdout_path))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn tail(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(MAX_GIT_OUTPUT)))?;
    let mut bytes = Vec::new();
    file.take(MAX_GIT_OUTPUT).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn symlink_within(base: &Path, path: &Path) -> Option<PathBuf> {
    let mut prefix = base.to_owned();
    for part in path.strip_prefix(base).ok()?.iter() {
        prefix.push(part);
        if fs::symlink_metadata(&prefix).is_ok_and(|metadata| metadata.is_symlink()) {
            return Some(prefix);
        }
    }
    None
}

pub(super) struct FoundSkill {
    pub name: SkillName,
    pub dir: PathBuf,
    pub folder: String,
}

struct Finder<'a> {
    base: &'a Path,
    found: Vec<FoundSkill>,
    seen: BTreeSet<SkillName>,
    parsed: BTreeSet<PathBuf>,
    visited: usize,
    warnings: Vec<String>,
    cancellation: &'a CancellationToken,
}

pub(super) fn find_skills(
    base: &Path,
    subpath: Option<&str>,
    cancellation: &CancellationToken,
) -> Result<(Vec<FoundSkill>, Vec<String>), SkillsCliError> {
    let search = match subpath {
        Some(subpath) => base.join(subpath),
        None => base.to_owned(),
    };
    if let Some(link) = symlink_within(base, &search) {
        return Err(SkillsCliError::Failed(format!(
            "{} is a symlink; skills are installed only from real directories",
            shown(link.strip_prefix(base).unwrap_or(&link).display())
        )));
    }
    let canonical_base = base.canonicalize().map_err(at(base))?;
    let canonical_search = search.canonicalize().map_err(|error| {
        SkillsCliError::Failed(format!("{} is not readable: {error}", search.display()))
    })?;
    if !canonical_search.starts_with(&canonical_base) || !canonical_search.is_dir() {
        return Err(SkillsCliError::Failed(format!(
            "{} is not a directory inside the source",
            search.display()
        )));
    }
    let mut finder = Finder {
        base,
        found: Vec::new(),
        seen: BTreeSet::new(),
        parsed: BTreeSet::new(),
        visited: 0,
        warnings: Vec::new(),
        cancellation,
    };
    if finder.try_add(&search)? && !finder.found.is_empty() {
        return Ok((finder.found, finder.warnings));
    }
    let priority = [
        (search.clone(), 1),
        (search.join("skills"), 3),
        (search.join("skills/.curated"), 3),
        (search.join("skills/.experimental"), 3),
        (search.join("skills/.system"), 3),
        (search.join(".agents/skills"), 3),
        (search.join(".claude/skills"), 3),
    ];
    for (dir, depth) in priority {
        finder.walk(&dir, depth, 1)?;
    }
    if finder.found.is_empty() {
        finder.walk(&search, MAX_SCAN_DEPTH, 1)?;
    }
    Ok((finder.found, finder.warnings))
}

impl Finder<'_> {
    fn walk(&mut self, dir: &Path, max_depth: usize, depth: usize) -> Result<(), SkillsCliError> {
        if symlink_within(self.base, dir).is_some() {
            return Ok(());
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return Ok(());
        };
        let mut children: Vec<_> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.path())
            .collect();
        children.sort();
        for child in children {
            if self.cancellation.is_cancelled() {
                return Err(SkillsCliError::Cancelled);
            }
            if self.visited >= MAX_SCANNED_DIRS {
                self.warnings.push(format!(
                    "stopped after scanning {MAX_SCANNED_DIRS} directories"
                ));
                return Ok(());
            }
            self.visited += 1;
            let skipped = child
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| SKIP_DIRS.contains(&name));
            if self.try_add(&child)? || depth >= max_depth || skipped {
                continue;
            }
            self.walk(&child, max_depth, depth + 1)?;
        }
        Ok(())
    }

    fn try_add(&mut self, dir: &Path) -> Result<bool, SkillsCliError> {
        let label = || shown(dir.strip_prefix(self.base).unwrap_or(dir).display());
        let contents = match read_skill_file(dir) {
            Ok(Some(contents)) => contents,
            Ok(None) => return Ok(false),
            Err(issue) => {
                self.warnings
                    .push(format!("skipped {}: {}", label(), shown(issue)));
                return Ok(true);
            }
        };
        if !self.parsed.insert(dir.to_owned()) {
            return Ok(true);
        }
        let parsed = frontmatter::split(&contents)
            .ok_or(SkillIssue::MissingFrontmatter)
            .and_then(|document| frontmatter::parse(document.frontmatter));
        let fields = match parsed {
            Ok(fields) => fields,
            Err(issue) => {
                self.warnings
                    .push(format!("skipped {}: {}", label(), shown(issue)));
                return Ok(true);
            }
        };
        if fields
            .issues
            .iter()
            .any(|issue| matches!(issue, SkillIssue::MetadataValueNotString { key } if key == "internal"))
        {
            return Ok(true);
        }
        let (Some(name), Some(description)) = (fields.name, fields.description) else {
            self.warnings.push(format!(
                "skipped {}: frontmatter needs a name and a description",
                label()
            ));
            return Ok(true);
        };
        if description.is_empty() {
            self.warnings
                .push(format!("skipped {}: description is empty", label()));
            return Ok(true);
        }
        let name = match SkillName::parse(&name) {
            Ok(name) => name,
            Err(issue) => {
                self.warnings
                    .push(format!("skipped {}: {}", label(), shown(issue)));
                return Ok(true);
            }
        };
        let folder = dir.strip_prefix(self.base).unwrap_or(Path::new(""));
        let Some(folder) = folder
            .iter()
            .map(OsStr::to_str)
            .collect::<Option<Vec<_>>>()
            .map(|parts| parts.join("/"))
        else {
            self.warnings
                .push(format!("skipped {}: path is not UTF-8", label()));
            return Ok(true);
        };
        if self.seen.insert(name.clone()) {
            self.found.push(FoundSkill {
                name,
                dir: dir.to_owned(),
                folder,
            });
        }
        Ok(true)
    }
}

use std::borrow::Cow;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use serde::Serialize;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::install;
use super::{
    Catalog, LockEntry, RejectedSkill, SkillDirName, SkillIssue, SkillRecord, SkillRoots, discover,
    read_body,
};
use crate::host::settings::{SettingsError, read_skills_settings};

#[derive(Args)]
pub struct SkillsArguments {
    #[command(subcommand)]
    command: SkillsCommand,
}

#[derive(Subcommand)]
enum SkillsCommand {
    #[command(about = "List Agent Skills in the project, global and extra roots")]
    List(ListArguments),
    #[command(about = "Show one skill's frontmatter, source and issues")]
    Show(ShowArguments),
    #[command(
        about = "Install skills from GitHub, a git URL or a local directory into ~/.agents/skills"
    )]
    Add(AddArguments),
    #[command(about = "Update git-installed global skills whose upstream folder changed")]
    Update(UpdateArguments),
    #[command(about = "Remove global skills, their lock entries and agent links to them")]
    Remove(RemoveArguments),
}

#[derive(Args)]
pub(super) struct AddArguments {
    #[arg(
        help = "owner/repo[/path], owner/repo@skill, github:owner/repo, a GitHub tree URL, an https or ssh .git URL, or an absolute path"
    )]
    pub source: String,
    #[arg(long = "skill", value_name = "NAME")]
    pub skills: Vec<String>,
    #[arg(long)]
    pub all: bool,
    #[arg(long = "ref", value_name = "REF")]
    pub reference: Option<String>,
    #[arg(
        long,
        short = 'y',
        help = "Replace an existing directory not installed from this source"
    )]
    pub yes: bool,
}

#[derive(Args)]
pub(super) struct UpdateArguments {
    pub names: Vec<String>,
    #[arg(long, help = "Only report which skills have upstream changes")]
    pub check: bool,
}

#[derive(Args)]
pub(super) struct RemoveArguments {
    #[arg(required = true)]
    pub names: Vec<String>,
    #[arg(long, short = 'y')]
    pub yes: bool,
    #[arg(long, value_name = "PATH")]
    workspace: Option<PathBuf>,
}

#[derive(Args)]
struct ListArguments {
    #[arg(long)]
    json: bool,
    #[arg(long, value_name = "PATH")]
    workspace: Option<PathBuf>,
}

#[derive(Args)]
struct ShowArguments {
    name: String,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    body: bool,
    #[arg(long, value_name = "PATH")]
    workspace: Option<PathBuf>,
}

#[derive(Debug, Error)]
pub enum SkillsCliError {
    #[error("HOME is unavailable; skills are discovered under $HOME/.agents/skills")]
    Home,
    #[error("workspace {0} must be an existing directory")]
    Workspace(PathBuf),
    #[error("skills settings could not be read: {0}")]
    Settings(#[from] SettingsError),
    #[error("no skill named {0:?}; run `vorra skills list`")]
    NotFound(String),
    #[error("SKILL.md body could not be read: {0}")]
    Body(SkillIssue),
    #[error("skills command was cancelled")]
    Cancelled,
    #[error(
        "source {input:?} is not fetched natively: {reason}; run `npx skills add {input} -g -a codex -y`"
    )]
    UnsupportedSource { input: String, reason: &'static str },
    #[error("git was not found on PATH; install git or run `{0}`")]
    GitMissing(String),
    #[error("git {0} timed out after 5 minutes")]
    GitTimeout(&'static str),
    #[error("git {action} failed: {message}")]
    Git {
        action: &'static str,
        message: String,
    },
    #[error("another vorra skills add, update or remove is running")]
    Busy,
    #[error("{name:?} is in the read-only root {root}; run `{command}`")]
    ReadOnly {
        name: String,
        root: PathBuf,
        command: String,
    },
    #[error("{0}")]
    Failed(String),
    #[error("could not write skills output: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub fn run(
    arguments: SkillsArguments,
    state_dir: &Path,
    cancellation: &CancellationToken,
) -> Result<(), SkillsCliError> {
    let home = home()?;
    let (json, workspace) = match &arguments.command {
        SkillsCommand::List(list) => (list.json, list.workspace.clone()),
        SkillsCommand::Show(show) => (show.json, show.workspace.clone()),
        SkillsCommand::Add(add) => return install::add(&home, add, cancellation),
        SkillsCommand::Update(update) => return install::update(&home, update, cancellation),
        SkillsCommand::Remove(remove) => {
            let roots = roots(&home, state_dir, remove.workspace.clone())?;
            let catalog = discover(&roots, cancellation).map_err(|_| SkillsCliError::Cancelled)?;
            return install::remove(&home, &catalog, &roots.extra, remove);
        }
    };
    install::recover_if_idle(&home);
    let catalog = discover(&roots(&home, state_dir, workspace)?, cancellation)
        .map_err(|_| SkillsCliError::Cancelled)?;
    let mut stderr = io::stderr().lock();
    for warning in &catalog.warnings {
        writeln!(stderr, "vorra: warning: {}", shown(warning))?;
    }
    let mut stdout = io::stdout().lock();
    match arguments.command {
        SkillsCommand::List(_) => list(&catalog, json, &mut stdout),
        SkillsCommand::Show(show) => self::show(&catalog, &show, &mut stdout),
        SkillsCommand::Add(_) | SkillsCommand::Update(_) | SkillsCommand::Remove(_) => Ok(()),
    }
}

fn printable(text: &str, keep_lines: bool) -> Cow<'_, str> {
    let escaped = |character: char| {
        character.is_control() && !(keep_lines && matches!(character, '\n' | '\t'))
    };
    if !text.chars().any(escaped) {
        return Cow::Borrowed(text);
    }
    let mut output = String::with_capacity(text.len() + 8);
    for character in text.chars() {
        if escaped(character) {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    Cow::Owned(output)
}

pub(super) fn shown(value: impl fmt::Display) -> String {
    printable(&value.to_string(), false).into_owned()
}

fn home() -> Result<PathBuf, SkillsCliError> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or(SkillsCliError::Home)
}

fn roots(
    home: &Path,
    state_dir: &Path,
    workspace: Option<PathBuf>,
) -> Result<SkillRoots, SkillsCliError> {
    let workspace = match workspace {
        Some(workspace) => {
            let absolute = std::path::absolute(&workspace)?;
            if !absolute.is_dir() {
                return Err(SkillsCliError::Workspace(workspace));
            }
            absolute
        }
        None => std::env::current_dir()?,
    };
    let settings = read_skills_settings(state_dir)?;
    Ok(SkillRoots::new(
        home.to_owned(),
        Some(workspace),
        settings.extra_roots,
    ))
}

#[derive(Serialize)]
struct ListedSkill<'a> {
    name: &'a str,
    path: Option<&'a Path>,
    scope: &'static str,
    agents: [&'static str; 0],
    source: Option<&'a str>,
    #[serde(rename = "sourceUrl")]
    source_url: Option<&'a str>,
    #[serde(rename = "sourceType")]
    source_type: Option<&'a str>,
    status: &'static str,
    root: Option<&'static str>,
    root_path: Option<&'a Path>,
    dir_name: Option<&'a SkillDirName>,
    description: Option<&'a str>,
    offered: bool,
    shadowed: bool,
    issues: &'a [SkillIssue],
}

type SourceColumns<'a> = (Option<&'a str>, Option<&'a str>, Option<&'a str>);

fn source(entry: Option<&LockEntry>) -> SourceColumns<'_> {
    (
        entry.map(|entry| entry.source.as_str()),
        entry.and_then(|entry| entry.source_url.as_deref()),
        entry.map(|entry| entry.source_type.as_str()),
    )
}

fn listed(catalog: &Catalog) -> Vec<ListedSkill<'_>> {
    let mut rows = Vec::new();
    for record in &catalog.skills {
        let (source, source_url, source_type) = source(record.source.as_ref());
        rows.push(ListedSkill {
            name: record.name.as_str(),
            path: Some(&record.dir),
            scope: record.root.scope(),
            agents: [],
            source,
            source_url,
            source_type,
            status: if record.source.is_some() {
                "tracked"
            } else {
                "untracked"
            },
            root: Some(record.root.label()),
            root_path: Some(record.root.path()),
            dir_name: Some(&record.dir_name),
            description: Some(&record.description),
            offered: record.offered(),
            shadowed: record.shadowed,
            issues: &record.issues,
        });
    }
    for rejected in &catalog.rejected {
        rows.push(ListedSkill {
            name: &rejected.label,
            path: Some(&rejected.dir),
            scope: rejected.root.scope(),
            agents: [],
            source: None,
            source_url: None,
            source_type: None,
            status: "rejected",
            root: Some(rejected.root.label()),
            root_path: Some(rejected.root.path()),
            dir_name: None,
            description: None,
            offered: false,
            shadowed: false,
            issues: &rejected.issues,
        });
    }
    for missing in &catalog.missing {
        let (source, source_url, source_type) = source(Some(&missing.entry));
        rows.push(ListedSkill {
            name: &missing.name,
            path: None,
            scope: missing.scope,
            agents: [],
            source,
            source_url,
            source_type,
            status: "missing",
            root: None,
            root_path: None,
            dir_name: None,
            description: None,
            offered: false,
            shadowed: false,
            issues: &[],
        });
    }
    rows
}

fn list(catalog: &Catalog, json: bool, output: &mut impl Write) -> Result<(), SkillsCliError> {
    let rows = listed(catalog);
    if json {
        serde_json::to_writer_pretty(&mut *output, &rows)?;
        writeln!(output)?;
        return Ok(());
    }
    if rows.is_empty() {
        writeln!(output, "No skills found.")?;
        return Ok(());
    }
    let cells: Vec<[String; 4]> = rows
        .iter()
        .map(|row| {
            let mut root = row.root.unwrap_or("-").to_owned();
            if row.shadowed {
                root.push_str(" (shadowed)");
            }
            let status = match row.status {
                "missing" => "missing from disk".to_owned(),
                "rejected" => "rejected".to_owned(),
                _ => shown(row.source.unwrap_or("local")),
            };
            [
                shown(row.name),
                root,
                status,
                row.path
                    .map_or_else(|| "-".to_owned(), |path| shown(path.display())),
            ]
        })
        .collect();
    let mut widths = [4, 4, 6];
    for cell in &cells {
        for (width, value) in widths.iter_mut().zip(cell) {
            *width = (*width).max(value.chars().count());
        }
    }
    writeln!(
        output,
        "{:<w0$}  {:<w1$}  {:<w2$}  PATH",
        "NAME",
        "ROOT",
        "SOURCE",
        w0 = widths[0],
        w1 = widths[1],
        w2 = widths[2]
    )?;
    for (cell, row) in cells.iter().zip(&rows) {
        writeln!(
            output,
            "{:<w0$}  {:<w1$}  {:<w2$}  {}",
            cell[0],
            cell[1],
            cell[2],
            cell[3],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2]
        )?;
        for issue in row.issues {
            writeln!(output, "  ! {}", shown(issue))?;
        }
    }
    Ok(())
}

enum Found<'a> {
    Skill(&'a SkillRecord),
    Rejected(&'a RejectedSkill),
}

fn find<'a>(catalog: &'a Catalog, name: &str) -> Option<Found<'a>> {
    let wanted = SkillDirName::sanitize(name);
    let folder = |dir: &Path| {
        SkillDirName::sanitize(
            &dir.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        )
    };
    let winners = || catalog.skills.iter().filter(|record| !record.shadowed);
    winners()
        .find(|record| record.name.dir_name() == wanted)
        .or_else(|| winners().find(|record| folder(&record.dir) == wanted))
        .map(Found::Skill)
        .or_else(|| {
            catalog
                .rejected
                .iter()
                .find(|rejected| {
                    SkillDirName::sanitize(&rejected.label) == wanted
                        || folder(&rejected.dir) == wanted
                })
                .map(Found::Rejected)
        })
}

#[derive(Serialize)]
struct ShownSkill<'a> {
    #[serde(flatten)]
    record: &'a SkillRecord,
    offered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
}

#[derive(Serialize)]
struct ShownRejected<'a> {
    #[serde(flatten)]
    rejected: &'a RejectedSkill,
    offered: bool,
}

fn show(
    catalog: &Catalog,
    arguments: &ShowArguments,
    output: &mut impl Write,
) -> Result<(), SkillsCliError> {
    let found = find(catalog, &arguments.name)
        .ok_or_else(|| SkillsCliError::NotFound(arguments.name.clone()))?;
    let record = match found {
        Found::Rejected(rejected) => {
            if arguments.json {
                serde_json::to_writer_pretty(
                    &mut *output,
                    &ShownRejected {
                        rejected,
                        offered: false,
                    },
                )?;
                writeln!(output)?;
            } else {
                writeln!(output, "name: {}", shown(&rejected.label))?;
                writeln!(output, "path: {}", shown(rejected.dir.display()))?;
                writeln!(
                    output,
                    "root: {} ({})",
                    rejected.root.label(),
                    shown(rejected.root.path().display())
                )?;
                writeln!(output, "offered: no")?;
                writeln!(output, "issues:")?;
                for issue in &rejected.issues {
                    writeln!(output, "  - {}", shown(issue))?;
                }
            }
            return Ok(());
        }
        Found::Skill(record) => record,
    };
    let body = if arguments.body {
        Some(read_body(record).map_err(SkillsCliError::Body)?)
    } else {
        None
    };
    if arguments.json {
        serde_json::to_writer_pretty(
            &mut *output,
            &ShownSkill {
                record,
                offered: record.offered(),
                body,
            },
        )?;
        writeln!(output)?;
        return Ok(());
    }
    writeln!(output, "name: {}", shown(&record.name))?;
    writeln!(output, "directory name: {}", record.dir_name)?;
    writeln!(output, "path: {}", shown(record.dir.display()))?;
    writeln!(
        output,
        "root: {} ({})",
        record.root.label(),
        shown(record.root.path().display())
    )?;
    writeln!(output, "description: {}", shown(&record.description))?;
    if let Some(license) = &record.license {
        writeln!(output, "license: {}", shown(license))?;
    }
    if let Some(compatibility) = &record.compatibility {
        writeln!(output, "compatibility: {}", shown(compatibility))?;
    }
    if let Some(allowed_tools) = &record.allowed_tools {
        writeln!(
            output,
            "allowed-tools (never applied): {}",
            shown(allowed_tools)
        )?;
    }
    for (key, value) in &record.metadata {
        writeln!(output, "metadata.{}: {}", shown(key), shown(value))?;
    }
    match &record.source {
        Some(entry) => {
            writeln!(
                output,
                "source: {} ({})",
                shown(&entry.source),
                shown(&entry.source_type)
            )?;
            if let Some(url) = &entry.source_url {
                writeln!(output, "source url: {}", shown(url))?;
            }
            if let Some(reference) = &entry.reference {
                writeln!(output, "ref: {}", shown(reference))?;
            }
            if let Some(hash) = entry
                .skill_folder_hash
                .as_ref()
                .or(entry.computed_hash.as_ref())
            {
                writeln!(output, "folder hash: {}", shown(hash))?;
            }
            if let Some(updated) = &entry.updated_at {
                writeln!(output, "updated: {}", shown(updated))?;
            }
        }
        None => writeln!(output, "source: local (no lock entry)")?,
    }
    writeln!(
        output,
        "offered: {}",
        if record.offered() { "yes" } else { "no" }
    )?;
    if !record.issues.is_empty() {
        writeln!(output, "issues:")?;
        for issue in &record.issues {
            writeln!(output, "  - {}", shown(issue))?;
        }
    }
    if let Some(body) = body {
        writeln!(output)?;
        let body = printable(&body, true);
        output.write_all(body.as_bytes())?;
        if !body.ends_with('\n') {
            writeln!(output)?;
        }
    }
    Ok(())
}

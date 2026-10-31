use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use serde::Serialize;
use thiserror::Error;

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
    #[error("could not serialize skills output")]
    Serialization,
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub fn run(arguments: SkillsArguments, state_dir: &Path) -> Result<(), SkillsCliError> {
    let (json, workspace) = match &arguments.command {
        SkillsCommand::List(list) => (list.json, list.workspace.clone()),
        SkillsCommand::Show(show) => (show.json, show.workspace.clone()),
    };
    let catalog = load(state_dir, workspace)?;
    let mut stderr = io::stderr().lock();
    for warning in &catalog.warnings {
        writeln!(stderr, "vorra: warning: {warning}")?;
    }
    let mut stdout = io::stdout().lock();
    match arguments.command {
        SkillsCommand::List(_) => list(&catalog, json, &mut stdout),
        SkillsCommand::Show(show) => self::show(&catalog, &show, &mut stdout),
    }
}

fn load(state_dir: &Path, workspace: Option<PathBuf>) -> Result<Catalog, SkillsCliError> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or(SkillsCliError::Home)?;
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
    Ok(discover(&SkillRoots::new(
        home,
        Some(workspace),
        settings.extra_roots,
    )))
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
        serde_json::to_writer_pretty(&mut *output, &rows)
            .map_err(|_| SkillsCliError::Serialization)?;
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
                _ => row.source.unwrap_or("local").to_owned(),
            };
            [
                row.name.to_owned(),
                root,
                status,
                row.path
                    .map_or_else(|| "-".to_owned(), |path| path.display().to_string()),
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
            writeln!(output, "  ! {issue}")?;
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
                )
                .map_err(|_| SkillsCliError::Serialization)?;
                writeln!(output)?;
            } else {
                writeln!(output, "name: {}", rejected.label)?;
                writeln!(output, "path: {}", rejected.dir.display())?;
                writeln!(
                    output,
                    "root: {} ({})",
                    rejected.root.label(),
                    rejected.root.path().display()
                )?;
                writeln!(output, "offered: no")?;
                writeln!(output, "issues:")?;
                for issue in &rejected.issues {
                    writeln!(output, "  - {issue}")?;
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
        )
        .map_err(|_| SkillsCliError::Serialization)?;
        writeln!(output)?;
        return Ok(());
    }
    writeln!(output, "name: {}", record.name)?;
    writeln!(output, "directory name: {}", record.dir_name)?;
    writeln!(output, "path: {}", record.dir.display())?;
    writeln!(
        output,
        "root: {} ({})",
        record.root.label(),
        record.root.path().display()
    )?;
    writeln!(output, "description: {}", record.description)?;
    if let Some(license) = &record.license {
        writeln!(output, "license: {license}")?;
    }
    if let Some(compatibility) = &record.compatibility {
        writeln!(output, "compatibility: {compatibility}")?;
    }
    if let Some(allowed_tools) = &record.allowed_tools {
        writeln!(output, "allowed-tools (never applied): {allowed_tools}")?;
    }
    for (key, value) in &record.metadata {
        writeln!(output, "metadata.{key}: {value}")?;
    }
    match &record.source {
        Some(entry) => {
            writeln!(output, "source: {} ({})", entry.source, entry.source_type)?;
            if let Some(url) = &entry.source_url {
                writeln!(output, "source url: {url}")?;
            }
            if let Some(reference) = &entry.reference {
                writeln!(output, "ref: {reference}")?;
            }
            if let Some(hash) = entry
                .skill_folder_hash
                .as_ref()
                .or(entry.computed_hash.as_ref())
            {
                writeln!(output, "folder hash: {hash}")?;
            }
            if let Some(updated) = &entry.updated_at {
                writeln!(output, "updated: {updated}")?;
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
            writeln!(output, "  - {issue}")?;
        }
    }
    if let Some(body) = body {
        writeln!(output)?;
        output.write_all(body.as_bytes())?;
        if !body.ends_with('\n') {
            writeln!(output)?;
        }
    }
    Ok(())
}

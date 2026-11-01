use std::io;
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags};

use super::SkillIssue;
use super::discover::{read_file_at, trusted_status};

pub const MAX_PROJECT_INSTRUCTIONS_BYTES: u64 = 32 * 1024;
const FILE_NAME: &str = "AGENTS.md";

pub struct ProjectInstructions {
    pub path: PathBuf,
    pub text: String,
    pub truncated_from: Option<u64>,
}

pub fn project_instructions(
    start: &Path,
    warnings: &mut Vec<String>,
) -> Option<ProjectInstructions> {
    for dir in start.ancestors() {
        let path = dir.join(FILE_NAME);
        if std::fs::symlink_metadata(&path).is_ok() {
            return match load(dir) {
                Ok(Some((text, truncated_from))) => Some(ProjectInstructions {
                    path,
                    text,
                    truncated_from,
                }),
                Ok(None) => None,
                Err(reason) => {
                    warnings.push(format!("{} was not loaded: {reason}", path.display()));
                    None
                }
            };
        }
        if std::fs::symlink_metadata(dir.join(".git")).is_ok() {
            return None;
        }
    }
    None
}

fn load(dir: &Path) -> Result<Option<(String, Option<u64>)>, String> {
    let directory = rustix::fs::open(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| io::Error::from(error).to_string())?;
    let status = rustix::fs::fstat(&directory).map_err(|error| error.to_string())?;
    trusted_status(&status).map_err(|reason| format!("its directory is {reason}"))?;
    read_file_at(
        &directory,
        FILE_NAME,
        MAX_PROJECT_INSTRUCTIONS_BYTES,
        true,
        true,
    )
    .map_err(|issue| match issue {
        SkillIssue::SkillFileNotRegular => "not a regular file; symlinks are refused".to_owned(),
        SkillIssue::Untrusted { reason } => reason.to_owned(),
        SkillIssue::NotUtf8 => "not UTF-8".to_owned(),
        other => other.to_string(),
    })
}

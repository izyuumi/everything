use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

const GUIDES: &[(&str, &[(&str, &str)])] = &[
    (
        "macos",
        &[
            ("docs/development/releases.md", "Install"),
            (
                "docs/development/releases.md",
                "First Vorra release: install manually once",
            ),
            ("docs/development/tui.md", "First-run onboarding"),
            ("docs/development/host.md", "macOS service management"),
            (
                "docs/development/macos-client.md",
                "Build and run the macOS client",
            ),
        ],
    ),
    (
        "linux",
        &[
            ("docs/development/releases.md", "Install"),
            ("docs/development/tui.md", "First-run onboarding"),
            (
                "docs/development/host.md",
                "Headless host and owner commands",
            ),
            ("docs/development/cli.md", "Initialize and inspect"),
        ],
    ),
    (
        "ios",
        &[
            (
                "docs/development/ios-client.md",
                "Current-main integration and prerequisites",
            ),
            ("docs/development/ios-client.md", "Pairing and trust"),
            ("docs/development/ios-client.md", "TestFlight distribution"),
        ],
    ),
];

struct Section {
    doc: usize,
    heading: String,
    start: usize,
    end: usize,
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root");
    for path in ["README.md", "CHANGELOG.md", "docs"] {
        println!("cargo:rerun-if-changed={}", root.join(path).display());
    }
    let mut paths = Vec::new();
    collect(&root, &root.join("docs"), &mut paths);
    paths.sort();
    paths.splice(0..0, ["README.md".to_owned(), "CHANGELOG.md".to_owned()]);

    let mut output = String::from("pub static DOCS: &[Doc] = &[\n");
    let mut sections = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let file = root.join(path);
        let text = fs::read_to_string(&file).unwrap_or_else(|error| panic!("{path}: {error}"));
        let before = sections.len();
        split(index, &text, &mut sections);
        let title = sections[before..]
            .first()
            .map_or(path.as_str(), |section: &Section| section.heading.as_str());
        let file = file.to_str().expect("UTF-8 documentation path");
        writeln!(
            output,
            "    Doc {{ path: {path:?}, title: {title:?}, text: include_str!({file:?}) }},"
        )
        .expect("write");
    }
    output.push_str("];\n\npub static SECTIONS: &[Section] = &[\n");
    for section in &sections {
        writeln!(
            output,
            "    Section {{ doc: {}, heading: {:?}, start: {}, end: {} }},",
            section.doc, section.heading, section.start, section.end
        )
        .expect("write");
    }
    output.push_str("];\n\npub static GUIDES: &[(&str, &[usize])] = &[\n");
    for (os, references) in GUIDES {
        let indexes: Vec<String> = references
            .iter()
            .map(|(path, heading)| {
                sections
                    .iter()
                    .position(|section| paths[section.doc] == *path && section.heading == *heading)
                    .unwrap_or_else(|| {
                        panic!("install guide section {path} \"{heading}\" no longer exists")
                    })
                    .to_string()
            })
            .collect();
        writeln!(output, "    ({os:?}, &[{}]),", indexes.join(", ")).expect("write");
    }
    output.push_str("];\n");
    let destination = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    fs::write(destination.join("content.rs"), output).expect("write generated content");
}

fn collect(root: &Path, directory: &Path, paths: &mut Vec<String>) {
    for entry in fs::read_dir(directory).expect("read documentation directory") {
        let path = entry.expect("documentation entry").path();
        if path.is_dir() {
            collect(root, &path, paths);
        } else if path.extension().is_some_and(|extension| extension == "md") {
            let relative = path
                .strip_prefix(root)
                .expect("relative documentation path");
            paths.push(
                relative
                    .to_str()
                    .expect("UTF-8 documentation path")
                    .replace('\\', "/"),
            );
        }
    }
}

fn split(doc: usize, text: &str, sections: &mut Vec<Section>) {
    let mut fenced = false;
    let mut offset = 0;
    let mut current: Option<Section> = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
        }
        let level = line.bytes().take_while(|byte| *byte == b'#').count();
        if !fenced && (1..=6).contains(&level) && line[level..].starts_with(' ') {
            if let Some(mut section) = current.take() {
                section.end = offset;
                sections.push(section);
            }
            current = Some(Section {
                doc,
                heading: line[level..].trim().trim_end_matches('#').trim().to_owned(),
                start: offset + line.len(),
                end: text.len(),
            });
        }
        offset += line.len();
    }
    if let Some(section) = current {
        sections.push(section);
    }
}

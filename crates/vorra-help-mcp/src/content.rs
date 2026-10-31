use std::sync::LazyLock;

pub struct Doc {
    pub path: &'static str,
    pub title: &'static str,
    pub text: &'static str,
}

pub struct Section {
    pub doc: usize,
    pub heading: &'static str,
    pub start: usize,
    pub end: usize,
}

include!(concat!(env!("OUT_DIR"), "/content.rs"));

pub const URI_PREFIX: &str = "vorra-docs://";
const SNIPPET_BYTES: usize = 320;
const MAX_TERMS: usize = 16;

static LOWERCASE: LazyLock<Vec<String>> = LazyLock::new(|| {
    DOCS.iter()
        .map(|doc| doc.text.to_ascii_lowercase())
        .collect()
});

impl Section {
    pub fn path(&self) -> &'static str {
        DOCS[self.doc].path
    }

    pub fn body(&self) -> &'static str {
        DOCS[self.doc].text[self.start..self.end].trim()
    }

    fn lowercase_body(&self) -> &str {
        &LOWERCASE[self.doc][self.start..self.end]
    }
}

pub fn doc(path: &str) -> Option<&'static Doc> {
    let path = path.strip_prefix(URI_PREFIX).unwrap_or(path);
    DOCS.iter().find(|doc| doc.path == path)
}

pub fn first_section(path: &str) -> Option<&'static Section> {
    SECTIONS.iter().find(|section| section.path() == path)
}

pub fn terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for term in query
        .split(|character: char| {
            !(character.is_alphanumeric() || matches!(character, '.' | '-' | '_'))
        })
        .map(|term| term.trim_matches(|character| matches!(character, '.' | '-' | '_')))
        .filter(|term| term.len() >= 2)
        .map(str::to_ascii_lowercase)
    {
        if !terms.contains(&term) {
            terms.push(term);
        }
        if terms.len() == MAX_TERMS {
            break;
        }
    }
    terms
}

pub fn search(
    terms: &[String],
    limit: usize,
    boost: impl Fn(&Section) -> f64,
) -> Vec<&'static Section> {
    let counts: Vec<Vec<(usize, usize)>> = SECTIONS
        .iter()
        .map(|section| {
            let heading = section.heading.to_ascii_lowercase();
            let body = section.lowercase_body();
            terms
                .iter()
                .map(|term| {
                    (
                        heading.matches(term.as_str()).count(),
                        body.matches(term.as_str()).count(),
                    )
                })
                .collect()
        })
        .collect();
    let sections = SECTIONS.len() as f64;
    let average = SECTIONS
        .iter()
        .map(|section| (section.end - section.start) as f64)
        .sum::<f64>()
        / sections.max(1.0);
    let rarity: Vec<f64> = (0..terms.len())
        .map(|term| {
            let found = counts
                .iter()
                .filter(|count| count[term].0 + count[term].1 > 0)
                .count() as f64;
            (1.0 + (sections - found + 0.5) / (found + 0.5)).ln()
        })
        .collect();
    let mut hits: Vec<(usize, f64)> = counts
        .iter()
        .enumerate()
        .filter_map(|(index, count)| {
            let section = &SECTIONS[index];
            let length = (section.end - section.start) as f64 / average.max(1.0);
            let mut matched = 0;
            let mut score = 0.0;
            for (term, (in_heading, in_body)) in count.iter().enumerate() {
                if in_heading + in_body == 0 {
                    continue;
                }
                matched += 1;
                let frequency = *in_body as f64;
                let body = frequency * 2.2 / (frequency + 1.2 * (0.25 + 0.75 * length));
                score += rarity[term] * (body + 2.0 * (*in_heading).min(1) as f64);
            }
            let coverage = matched as f64 / terms.len() as f64;
            (matched > 0).then(|| (index, score * coverage * coverage * boost(section)))
        })
        .collect();
    hits.sort_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
    hits.into_iter()
        .take(limit)
        .map(|(index, _)| &SECTIONS[index])
        .collect()
}

pub fn snippet(section: &Section, terms: &[String]) -> String {
    let body = section.body();
    let lowercase = body.to_ascii_lowercase();
    let found = terms
        .iter()
        .filter_map(|term| lowercase.find(term.as_str()))
        .min()
        .unwrap_or(0);
    let mut start = found.saturating_sub(SNIPPET_BYTES / 3);
    while !body.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + SNIPPET_BYTES).min(body.len());
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    let text = body[start..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    match (start > 0, end < body.len()) {
        (true, true) => format!("…{text}…"),
        (true, false) => format!("…{text}"),
        (false, true) => format!("{text}…"),
        (false, false) => text,
    }
}

//! Minimal OKF v0.2 conformance checking for wiki pages.
//!
//! This is the gate on every agent write. A model that mangles frontmatter must
//! produce a failed tool call it can retry, never a corrupt page on disk.
//!
//! Conformance per SPEC §11 is deliberately thin: a parseable YAML frontmatter
//! block with a non-empty `type`. We add one project-specific check — that
//! footnote labels resolve to `sources` ids — because unresolvable citations are
//! the failure mode that would quietly destroy provenance.

use anyhow::{bail, Result};
use std::collections::HashSet;

#[derive(Debug)]
pub struct Page {
    pub frontmatter: serde_yaml::Mapping,
    pub body: String,
}

impl Page {
    pub fn page_type(&self) -> Option<&str> {
        self.frontmatter
            .get(serde_yaml::Value::from("type"))
            .and_then(|value| value.as_str())
    }

    #[allow(dead_code)] // for index.md generation
    pub fn description(&self) -> Option<&str> {
        self.frontmatter
            .get(serde_yaml::Value::from("description"))
            .and_then(|value| value.as_str())
    }

    #[allow(dead_code)] // for index.md generation
    pub fn title(&self) -> Option<&str> {
        self.frontmatter
            .get(serde_yaml::Value::from("title"))
            .and_then(|value| value.as_str())
    }

    /// `(id, resource)` for every source entry that has an id.
    pub fn source_entries(&self) -> Vec<(String, String)> {
        let mut entries = Vec::new();
        if let Some(sources) = self
            .frontmatter
            .get(serde_yaml::Value::from("sources"))
            .and_then(|value| value.as_sequence())
        {
            for entry in sources {
                let Some(map) = entry.as_mapping() else { continue };
                let id = map
                    .get(serde_yaml::Value::from("id"))
                    .and_then(|v| v.as_str());
                let resource = map
                    .get(serde_yaml::Value::from("resource"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if let Some(id) = id {
                    entries.push((id.to_string(), resource.to_string()));
                }
            }
        }
        entries
    }

    pub fn source_ids(&self) -> HashSet<String> {
        let mut ids = HashSet::new();
        if let Some(sources) = self
            .frontmatter
            .get(serde_yaml::Value::from("sources"))
            .and_then(|value| value.as_sequence())
        {
            for entry in sources {
                if let Some(id) = entry
                    .as_mapping()
                    .and_then(|m| m.get(serde_yaml::Value::from("id")))
                    .and_then(|v| v.as_str())
                {
                    ids.insert(id.to_string());
                }
            }
        }
        ids
    }
}

/// The footnote label for an event id.
///
/// Uses the *last* 10 characters, not the first: a ULID begins with a
/// millisecond timestamp, so events written in the same sitting share a long
/// common prefix. Taking the front produced identical labels for different
/// events, which silently collapsed distinct citations into one.
pub fn footnote_label(event_id: &str) -> String {
    let suffix: String = event_id
        .chars()
        .rev()
        .take(10)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("e{suffix}")
}

/// Every markdown page under a directory, sorted, skipping dotfiles.
///
/// Dotfiles are skipped because editors write state into the wiki: Obsidian
/// creates `.obsidian/` merely by opening it.
pub fn pages(dir: &std::path::Path) -> Result<Vec<std::path::PathBuf>> {
    let mut found = Vec::new();
    if !dir.is_dir() {
        return Ok(found);
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            found.extend(pages(&path)?);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            found.push(path);
        }
    }
    found.sort();
    Ok(found)
}

/// A page's path relative to the wiki root, in OKF bundle-absolute form.
pub fn bundle_path(wiki: &std::path::Path, page: &std::path::Path) -> String {
    format!(
        "/{}",
        page.strip_prefix(wiki)
            .unwrap_or(page)
            .to_string_lossy()
            .replace('\\', "/")
    )
}

/// Split a document into frontmatter and body.
pub fn parse(content: &str) -> Result<Page> {
    let rest = content
        .strip_prefix("---\n")
        .ok_or_else(|| anyhow::anyhow!("page must begin with a `---` frontmatter delimiter"))?;
    let end = rest
        .find("\n---\n")
        .or_else(|| rest.strip_suffix("\n---").map(|s| s.len()))
        .ok_or_else(|| anyhow::anyhow!("frontmatter block is not closed with `---`"))?;

    let (yaml, body) = rest.split_at(end);
    let body = body.trim_start_matches("\n---\n").to_string();

    let value: serde_yaml::Value =
        serde_yaml::from_str(yaml).map_err(|error| anyhow::anyhow!("invalid YAML: {error}"))?;
    let frontmatter = match value {
        serde_yaml::Value::Mapping(map) => map,
        serde_yaml::Value::Null => serde_yaml::Mapping::new(),
        _ => bail!("frontmatter must be a YAML mapping"),
    };
    Ok(Page { frontmatter, body })
}

/// Full validation, used as the write gate.
pub fn validate(content: &str) -> Result<Page> {
    let page = parse(content)?;

    match page.page_type() {
        Some(value) if !value.trim().is_empty() => {}
        Some(_) => bail!("frontmatter `type` must not be empty"),
        None => bail!("frontmatter must contain a `type` field (OKF SPEC §11)"),
    }

    // Two sources sharing an id makes attribution ambiguous: a footnote pointing
    // at that id does not identify which event it came from.
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (id, resource) in page.source_entries() {
        if let Some(existing) = seen.get(&id) {
            if existing != &resource {
                bail!(
                    "two different sources both use id `{id}` ({existing} and \
                     {resource}); every source needs a distinct id or footnote \
                     attribution is ambiguous"
                );
            }
        }
        seen.insert(id, resource);
    }

    // Footnote labels must resolve to a sources id. Keyed rather than
    // positional attribution is the whole point (SPEC §5.1), so a dangling
    // label means provenance was lost.
    let ids = page.source_ids();
    for label in footnote_references(&page.body) {
        if !ids.contains(&label) {
            bail!(
                "footnote [^{label}] has no matching entry in `sources` \
                 (add one with `id: {label}`)"
            );
        }
    }
    Ok(page)
}

/// Bundle-absolute page links in a body, e.g. `[Ada](/people/ada.md)`.
///
/// Only internal links to markdown pages are returned: external URLs and
/// anchors are somebody else's problem.
pub fn page_links(body: &str) -> Vec<String> {
    let mut links = Vec::new();
    let bytes: Vec<char> = body.chars().collect();
    let mut index = 0;
    while index < bytes.len() {
        // Find `](` — the boundary between a markdown link's text and target.
        if bytes[index] == ']' && index + 1 < bytes.len() && bytes[index + 1] == '(' {
            let mut end = index + 2;
            while end < bytes.len() && bytes[end] != ')' {
                end += 1;
            }
            if end < bytes.len() {
                let target: String = bytes[index + 2..end].iter().collect();
                let target = target.split_whitespace().next().unwrap_or("").to_string();
                if target.starts_with('/') && target.ends_with(".md") {
                    links.push(target);
                }
            }
            index = end;
        }
        index += 1;
    }
    links
}

/// Footnote labels referenced in the body, e.g. `[^e01K2C5]`, excluding the
/// definition lines themselves.
fn footnote_references(body: &str) -> HashSet<String> {
    let mut labels = HashSet::new();
    for line in body.lines() {
        let trimmed = line.trim_start();
        let is_definition = trimmed.starts_with("[^") && trimmed.contains("]:");
        let mut rest = line;
        while let Some(start) = rest.find("[^") {
            let after = &rest[start + 2..];
            let Some(end) = after.find(']') else { break };
            let label = &after[..end];
            let definition_here = is_definition && after[end..].starts_with("]:");
            if !label.is_empty() && !definition_here {
                labels.insert(label.to_string());
            }
            rest = &after[end + 1..];
        }
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_minimal_page() {
        let page = validate("---\ntype: Person\n---\n\nHello.\n").unwrap();
        assert_eq!(page.page_type(), Some("Person"));
        assert_eq!(page.body.trim(), "Hello.");
    }

    #[test]
    fn rejects_missing_frontmatter() {
        assert!(validate("Just some markdown.\n").is_err());
    }

    #[test]
    fn rejects_unclosed_frontmatter() {
        assert!(validate("---\ntype: Person\n\nHello.\n").is_err());
    }

    #[test]
    fn rejects_missing_type() {
        let error = validate("---\ntitle: Ada\n---\n\nHi.\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("type"), "{error}");
    }

    #[test]
    fn rejects_invalid_yaml() {
        assert!(validate("---\ntype: [unclosed\n---\n\nHi.\n").is_err());
    }

    #[test]
    fn rejects_dangling_footnote() {
        let content = "---\ntype: Person\n---\n\nBirthday is 14 March.[^e01K2C5]\n";
        let error = validate(content).unwrap_err().to_string();
        assert!(error.contains("e01K2C5"), "{error}");
    }

    #[test]
    fn accepts_resolved_footnote() {
        let content = "---\ntype: Person\nsources:\n  - id: e01K2C5\n    resource: /log/2026-08.jsonl#01K2C5\n---\n\nBirthday is 14 March.[^e01K2C5]\n\n[^e01K2C5]: \"Ada's birthday is 14 March.\"\n";
        let page = validate(content).unwrap();
        assert!(page.source_ids().contains("e01K2C5"));
    }
}

#[cfg(test)]
mod label_tests {
    use super::*;

    /// The bug this guards against: two events recorded in the same millisecond
    /// share a long ULID prefix, so taking the front gave both the same label.
    #[test]
    fn labels_differ_for_events_in_the_same_millisecond() {
        let a = "01KZYYFAS6FQ8797S8MYJB4MVV";
        let b = "01KZYYFASQY0C0RB49XQT61J13";
        assert_eq!(&a[..8], &b[..8], "precondition: prefixes really do collide");
        assert_ne!(footnote_label(a), footnote_label(b));
    }

    #[test]
    fn label_is_stable_and_prefixed() {
        let label = footnote_label("01KZYYFAS6FQ8797S8MYJB4MVV");
        assert_eq!(label, "eS8MYJB4MVV");
        assert!(label.starts_with('e'));
        assert_eq!(label.len(), 11, "e + 10 characters");
    }

    #[test]
    fn rejects_two_sources_with_the_same_id() {
        let content = "---\ntype: Person\nsources:\n  - id: eDUPLICATE\n    resource: /log/a.jsonl#1\n  - id: eDUPLICATE\n    resource: /log/a.jsonl#2\n---\n\nHi.\n";
        let error = validate(content).unwrap_err().to_string();
        assert!(error.contains("eDUPLICATE"), "{error}");
        assert!(error.contains("ambiguous"), "{error}");
    }

    #[test]
    fn allows_the_same_source_listed_twice_identically() {
        let content = "---\ntype: Person\nsources:\n  - id: eSAME\n    resource: /log/a.jsonl#1\n  - id: eSAME\n    resource: /log/a.jsonl#1\n---\n\nHi.\n";
        assert!(validate(content).is_ok());
    }
}

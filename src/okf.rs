//! Minimal OKF v0.2 conformance checking for wiki pages (S21).
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

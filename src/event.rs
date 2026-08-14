use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// A log record. Field order here is the on-disk key order, and is deliberate:
/// `id`, `ts`, `text`, then the redaction tombstone. Keep it stable so diffs are.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub id: String,
    pub ts: String,
    /// `None` only for a redacted event, whose text has been destroyed.
    pub text: Option<String>,
    /// Reason, present only on redacted events. Absent from normal records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redacted: Option<String>,
}

impl Event {
    pub fn is_redacted(&self) -> bool {
        self.redacted.is_some()
    }

    /// The event's text, or a placeholder if it was redacted. Callers that must
    /// distinguish the two should check `is_redacted` first.
    #[allow(dead_code)]
    pub fn text(&self) -> &str {
        self.text.as_deref().unwrap_or("[redacted]")
    }

    /// Single-line rendering for list output.
    pub fn one_line(&self) -> String {
        match (&self.text, &self.redacted) {
            (Some(text), _) => text.replace('\n', "\\n"),
            (None, Some(reason)) => format!("[redacted: {reason}]"),
            (None, None) => "[missing text]".to_string(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if !is_ulid(&self.id) {
            bail!("invalid ULID `{}`", self.id);
        }
        if chrono::DateTime::parse_from_rfc3339(&self.ts).is_err() {
            bail!("invalid RFC 3339 timestamp `{}`", self.ts);
        }
        match (&self.text, &self.redacted) {
            (Some(text), None) => {
                if text.trim().is_empty() {
                    bail!("event text must not be empty");
                }
            }
            (None, Some(reason)) => {
                if reason.trim().is_empty() {
                    bail!("redaction reason must not be empty");
                }
            }
            (Some(_), Some(_)) => bail!("a redacted event must not retain its text"),
            (None, None) => bail!("event has no text and is not marked redacted"),
        }
        Ok(())
    }
}

fn is_ulid(value: &str) -> bool {
    value.len() == 26 && value.bytes().all(is_crockford)
}

fn is_crockford(c: u8) -> bool {
    matches!(c, b'0'..=b'9' | b'A'..=b'Z') && !matches!(c, b'I' | b'L' | b'O' | b'U')
}

pub fn normalize_prefix(prefix: &str) -> Result<String> {
    let prefix = prefix.to_ascii_uppercase();
    if prefix.len() < 4 {
        bail!("id prefix must contain at least 4 characters");
    }
    if prefix.len() > 26 || !prefix.bytes().all(is_crockford) {
        bail!("invalid ULID prefix `{prefix}`");
    }
    Ok(prefix)
}

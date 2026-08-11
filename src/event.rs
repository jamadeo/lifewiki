use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub id: String,
    pub ts: String,
    pub text: String,
}

impl Event {
    pub fn validate(&self) -> Result<()> {
        if self.id.len() != 26
            || !self.id.bytes().all(|c| {
                matches!(c, b'0'..=b'9' | b'A'..=b'Z') && !matches!(c, b'I' | b'L' | b'O' | b'U')
            })
        {
            bail!("invalid ULID `{}`", self.id);
        }
        if chrono::DateTime::parse_from_rfc3339(&self.ts).is_err() {
            bail!("invalid RFC 3339 timestamp `{}`", self.ts);
        }
        if self.text.trim().is_empty() {
            bail!("event text must not be empty");
        }
        Ok(())
    }
}

pub fn normalize_prefix(prefix: &str) -> Result<String> {
    let prefix = prefix.to_ascii_uppercase();
    if prefix.len() < 4 {
        bail!("id prefix must contain at least 4 characters");
    }
    if !prefix
        .bytes()
        .all(|c| matches!(c, b'0'..=b'9' | b'A'..=b'Z') && !matches!(c, b'I' | b'L' | b'O' | b'U'))
    {
        bail!("invalid ULID prefix `{prefix}`");
    }
    Ok(prefix)
}

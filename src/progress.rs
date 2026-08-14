//! Live output for agent runs.
//!
//! A run makes many slow model calls and edits files you care about, so it
//! narrates itself as it goes. Everything goes to stderr, leaving stdout for the
//! result — so `lw ask ... > answer.txt` keeps working.

use std::io::{IsTerminal, Write};

pub struct Reporter {
    enabled: bool,
    colour: bool,
}

impl Reporter {
    pub fn new() -> Self {
        Self {
            enabled: true,
            colour: std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    /// Prints nothing. For tests, and for `lw ask`, whose answer is the output.
    pub fn silent() -> Self {
        Self {
            enabled: false,
            colour: false,
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.colour {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn line(&self, text: &str) {
        let mut stderr = std::io::stderr();
        let _ = writeln!(stderr, "{text}");
        let _ = stderr.flush();
    }

    pub fn started(&self, events: usize, actor: &str) {
        if !self.enabled {
            return;
        }
        self.line(&format!(
            "{} {events} event{} with {}",
            self.paint("1;36", "folding"),
            if events == 1 { "" } else { "s" },
            self.paint("2", actor)
        ));
    }

    /// Printed before the call, since that is where the seconds go.
    pub fn thinking(&self, turn: usize) {
        if !self.enabled {
            return;
        }
        self.line(&format!(
            "\n{} {}",
            self.paint("2", &format!("[{turn}]")),
            self.paint("2", "thinking…")
        ));
    }

    pub fn model_text(&self, text: &str) {
        if !self.enabled || text.trim().is_empty() {
            return;
        }
        for line in text.trim().lines() {
            self.line(&format!("    {}", self.paint("2;3", line)));
        }
    }

    pub fn tool_call(&self, name: &str, arguments: &serde_json::Value) {
        if !self.enabled {
            return;
        }
        let summary = describe(name, arguments);
        self.line(&format!(
            "  {} {}{}",
            self.paint("1;34", "→"),
            self.paint("1", name),
            if summary.is_empty() {
                String::new()
            } else {
                format!(" {}", self.paint("36", &summary))
            }
        ));
    }

    pub fn tool_result(&self, output: &str, is_error: bool) {
        if !self.enabled {
            return;
        }
        // Errors print in full: watching the agent correct itself is the most
        // useful thing on screen when a run misbehaves.
        let (code, lines) = if is_error {
            ("1;31", output.trim().lines().collect::<Vec<_>>())
        } else {
            ("2", output.trim().lines().take(3).collect::<Vec<_>>())
        };
        let total = output.trim().lines().count();
        for line in &lines {
            self.line(&format!("    {}", self.paint(code, &truncate(line, 100))));
        }
        if total > lines.len() {
            self.line(&format!(
                "    {}",
                self.paint("2", &format!("… {} more lines", total - lines.len()))
            ));
        }
    }

    pub fn finished(&self, summary: &str) {
        if !self.enabled || summary.trim().is_empty() {
            return;
        }
        self.line(&format!(
            "  {} {}",
            self.paint("1;32", "✓"),
            self.paint("32", summary)
        ));
    }

    pub fn note(&self, text: &str) {
        if !self.enabled {
            return;
        }
        self.line(&format!("  {}", self.paint("2", text)));
    }

    pub fn warn(&self, text: &str) {
        if !self.enabled {
            return;
        }
        self.line(&format!("  {}", self.paint("33", text)));
    }
}

/// A short, human-readable form of a tool's arguments.
fn describe(name: &str, arguments: &serde_json::Value) -> String {
    let field = |key: &str| arguments.get(key).and_then(|value| value.as_str());
    match name {
        "search_wiki" => field("query")
            .map(|query| format!("{query:?}"))
            .unwrap_or_default(),
        "read_page" | "delete_page" => field("path").unwrap_or("").to_string(),
        "read_event" => field("id").unwrap_or("").to_string(),
        "write_page" => format!(
            "{} ({} bytes)",
            field("path").unwrap_or(""),
            field("content").map(str::len).unwrap_or(0)
        ),
        _ => String::new(),
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    format!("{}…", text.chars().take(limit).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_calls_compactly() {
        assert_eq!(
            describe("search_wiki", &serde_json::json!({"query": "apples"})),
            "\"apples\""
        );
        assert_eq!(
            describe(
                "write_page",
                &serde_json::json!({"path": "/a.md", "content": "12345"})
            ),
            "/a.md (5 bytes)"
        );
        assert_eq!(describe("finish", &serde_json::json!({})), "");
    }

    #[test]
    fn truncates_long_lines() {
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("ab", 5), "ab");
    }
}

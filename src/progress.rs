//! Live progress output for builds.
//!
//! A build makes many model calls and edits files you care about, so silence
//! for a minute followed by a summary is the wrong default. Everything the
//! agent does is printed as it happens, to stderr — stdout stays clean for the
//! final report, so `lw build | tee` still gives a usable summary.

use std::io::{IsTerminal, Write};
use std::sync::Mutex;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Verbosity {
    /// Only errors and the final summary.
    Quiet,
    /// Each tool call and its result. The default.
    Normal,
    /// Adds the model's prose, full arguments, and full tool output.
    Verbose,
}

pub struct Reporter {
    verbosity: Verbosity,
    colour: bool,
    /// Serialises writes so concurrent turns can't interleave mid-line.
    lock: Mutex<()>,
}

impl Reporter {
    pub fn new(verbosity: Verbosity) -> Self {
        Self {
            verbosity,
            colour: std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
            lock: Mutex::new(()),
        }
    }

    pub fn quiet() -> Self {
        Self::new(Verbosity::Quiet)
    }

    fn enabled(&self, level: Verbosity) -> bool {
        match (self.verbosity, level) {
            (Verbosity::Quiet, _) => false,
            (Verbosity::Normal, Verbosity::Verbose) => false,
            _ => true,
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
        let _guard = self.lock.lock().unwrap();
        let mut stderr = std::io::stderr();
        let _ = writeln!(stderr, "{text}");
        let _ = stderr.flush();
    }

    /// The build is about to start.
    pub fn build_started(&self, events: usize, actor: &str) {
        if !self.enabled(Verbosity::Normal) {
            return;
        }
        self.line(&format!(
            "{} {events} event{} with {}",
            self.paint("1;36", "folding"),
            if events == 1 { "" } else { "s" },
            self.paint("2", actor)
        ));
    }

    /// Waiting on the model. Printed before the call, since that's the slow bit.
    pub fn thinking(&self, turn: usize) {
        if !self.enabled(Verbosity::Normal) {
            return;
        }
        self.line(&format!(
            "\n{} {}",
            self.paint("2", &format!("[{turn}]")),
            self.paint("2", "thinking…")
        ));
    }

    /// The model said something in prose.
    pub fn model_text(&self, text: &str) {
        if !self.enabled(Verbosity::Verbose) || text.trim().is_empty() {
            return;
        }
        for line in text.trim().lines() {
            self.line(&format!("    {}", self.paint("2;3", line)));
        }
    }

    /// A tool is about to run.
    pub fn tool_call(&self, name: &str, arguments: &serde_json::Value) {
        if !self.enabled(Verbosity::Normal) {
            return;
        }
        let summary = if self.verbosity == Verbosity::Verbose {
            arguments.to_string()
        } else {
            describe(name, arguments)
        };
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

    /// A tool finished.
    pub fn tool_result(&self, output: &str, is_error: bool) {
        if !self.enabled(Verbosity::Normal) {
            return;
        }
        if is_error {
            // Always show the whole error: this is the agent correcting itself,
            // and it's the most useful thing on screen when a build misbehaves.
            for line in output.trim().lines() {
                self.line(&format!("    {}", self.paint("1;31", line)));
            }
            return;
        }
        if self.verbosity == Verbosity::Verbose {
            for line in output.trim().lines() {
                self.line(&format!("    {}", self.paint("2", line)));
            }
        } else {
            let lines: Vec<&str> = output.trim().lines().collect();
            let first = lines.first().copied().unwrap_or("");
            let suffix = if lines.len() > 1 {
                format!(" (+{} more lines)", lines.len() - 1)
            } else {
                String::new()
            };
            self.line(&format!(
                "    {}{}",
                self.paint("2", &truncate(first, 100)),
                self.paint("2", &suffix)
            ));
        }
    }

    /// The agent finished.
    pub fn finished(&self, summary: &str) {
        if !self.enabled(Verbosity::Normal) {
            return;
        }
        self.line(&format!(
            "  {} {}",
            self.paint("1;32", "✓"),
            self.paint("32", summary)
        ));
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
        "write_page" => {
            let path = field("path").unwrap_or("");
            let bytes = field("content").map(str::len).unwrap_or(0);
            format!("{path} ({bytes} bytes)")
        }
        "finish" => String::new(),
        _ => String::new(),
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}…")
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
    fn quiet_prints_nothing() {
        let reporter = Reporter::quiet();
        assert!(!reporter.enabled(Verbosity::Normal));
        assert!(!reporter.enabled(Verbosity::Verbose));
    }

    #[test]
    fn normal_excludes_verbose_only_output() {
        let reporter = Reporter::new(Verbosity::Normal);
        assert!(reporter.enabled(Verbosity::Normal));
        assert!(!reporter.enabled(Verbosity::Verbose));
    }

    #[test]
    fn truncates_long_lines() {
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("ab", 5), "ab");
    }
}

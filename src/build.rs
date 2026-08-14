//! `lw build` — fold new log events into the wiki.
//!
//! The transactional shape matters more than the model call. A build must be
//! idempotent (nothing new ⇒ no work, no commit), crash-safe (an interrupted
//! run leaves a dirty tree that `git checkout` discards, and the cursor
//! unmoved), and atomic (wiki, cursor and log move in one commit, so
//! `git revert` of a build is complete).

use crate::event::Event;
use crate::git;
use crate::log::Log;
use crate::model::{self, Backend};
use crate::reconcile::{Outcome, Reconciler};
use crate::tools::Workspace;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Records how far the wiki has been folded. Its absence means "never built".
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Cursor {
    /// Id of the last event folded into the wiki.
    pub last_event: Option<String>,
    /// When the last successful build finished.
    pub built_at: Option<String>,
}

impl Cursor {
    fn path(home: &Path) -> PathBuf {
        home.join("state/cursor.json")
    }

    pub fn load(home: &Path) -> Result<Self> {
        let path = Self::path(home);
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("{} is corrupt", path.display()))
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        let path = Self::path(home);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)? + "\n")?;
        Ok(())
    }
}

pub struct Options {
    /// Write the wiki here instead of `<home>/wiki` (S17: prompt comparison).
    pub into: Option<PathBuf>,
    /// Stop at this event id inclusive (S17: reproducible bounded replay).
    pub to: Option<String>,
    /// Start from an empty wiki and replay the whole log.
    pub rebuild: bool,
    /// Run the agent but discard changes and skip the commit.
    pub dry_run: bool,
    pub max_turns: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            into: None,
            to: None,
            rebuild: false,
            dry_run: false,
            max_turns: crate::reconcile::DEFAULT_MAX_TURNS,
        }
    }
}

#[derive(Debug)]
pub struct Report {
    pub events: usize,
    pub outcome: Option<Outcome>,
    pub run_dir: Option<PathBuf>,
    pub committed: bool,
}

pub fn build(home: &Path, options: Options) -> Result<Report> {
    let settings = model::ModelSettings::load(home)?;

    // Cheap checks first: a dirty tree or an up-to-date wiki should be
    // reported before we complain about a missing API key.
    preflight(home, &options)?;
    if pending_events(home, &options)?.is_empty() {
        return Ok(Report {
            events: 0,
            outcome: None,
            run_dir: None,
            committed: false,
        });
    }

    let backend = model::build(&settings)?;
    build_with(home, options, backend, &settings)
}

/// Refuse to start if the wiki has changes we would mix into our own commit.
fn preflight(home: &Path, options: &Options) -> Result<()> {
    if options.into.is_none() && !options.dry_run && git::is_dirty(home, "wiki")? {
        bail!(
            "wiki/ has uncommitted changes; commit or discard them first \
             (`git -C {} checkout -- wiki/`)",
            home.display()
        );
    }
    Ok(())
}

/// Events this build would fold, given the cursor and options.
fn pending_events(home: &Path, options: &Options) -> Result<Vec<Event>> {
    let log = Log::open(home)?;
    let cursor = if options.rebuild {
        Cursor::default()
    } else {
        Cursor::load(home)?
    };
    let mut events = match &cursor.last_event {
        Some(id) => log.since(id)?,
        None => log.events()?,
    };
    if let Some(limit) = &options.to {
        let resolved = log.resolve_prefix(limit)?;
        events.retain(|event| event.id <= resolved.id);
    }
    // A redacted event has no text to fold.
    events.retain(|event| !event.is_redacted());
    Ok(events)
}

/// The build, with the model supplied. Separated so tests can drive the whole
/// path — cursor, commit, run log — with a scripted model and no network.
pub fn build_with(
    home: &Path,
    options: Options,
    backend: std::sync::Arc<dyn Backend>,
    settings: &model::ModelSettings,
) -> Result<Report> {
    let target_is_default = options.into.is_none();
    preflight(home, &options)?;

    let wiki = options.into.clone().unwrap_or_else(|| home.join("wiki"));
    std::fs::create_dir_all(&wiki)?;

    let events = pending_events(home, &options)?;

    if events.is_empty() {
        return Ok(Report {
            events: 0,
            outcome: None,
            run_dir: None,
            committed: false,
        });
    }

    if options.rebuild && wiki.exists() {
        clear_markdown(&wiki)?;
    }

    let mut workspace = Workspace::new(&wiki)?;
    workspace.writable = true;

    let mut reconciler = Reconciler::new(backend.clone());
    reconciler.max_turns = options.max_turns;

    let started = chrono::Local::now();
    let result = reconciler
        .run(workspace, &events)
        .map(|(outcome, _)| outcome);

    // Write the run log whether or not the agent succeeded: a failed run is
    // exactly when the transcript is most useful.
    let run_dir = write_run_log(home, &started, &events, &result, backend.as_ref(), settings)
        .unwrap_or_else(|error| {
            eprintln!("warning: could not write run log: {error:#}");
            None
        });

    let outcome = result?;

    if options.dry_run {
        return Ok(Report {
            events: events.len(),
            outcome: Some(outcome),
            run_dir,
            committed: false,
        });
    }

    let last = events.last().expect("events is non-empty").id.clone();

    // Only the default target participates in the repo's history; `--into` is
    // a scratch materialisation for comparing prompts.
    if !target_is_default {
        return Ok(Report {
            events: events.len(),
            outcome: Some(outcome),
            run_dir,
            committed: false,
        });
    }

    let cursor = Cursor {
        last_event: Some(last),
        built_at: Some(started.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
    };
    cursor.save(home)?;

    let range = if events.len() == 1 {
        events[0].id[..8].to_string()
    } else {
        format!(
            "{}..{}",
            &events[0].id[..8],
            &events[events.len() - 1].id[..8]
        )
    };
    let message = format!(
        "wiki: fold {} event{} ({range})\n\n{}",
        events.len(),
        if events.len() == 1 { "" } else { "s" },
        outcome.summary
    );

    // One commit covering wiki, cursor and the log events being folded, so the
    // repo never records a wiki that claims to reflect events it hasn't seen.
    git::commit(home, &["wiki", "state/cursor.json", "log"], &message)?;

    Ok(Report {
        events: events.len(),
        outcome: Some(outcome),
        run_dir,
        committed: true,
    })
}

/// Remove every page, so a rebuild starts from nothing.
fn clear_markdown(wiki: &Path) -> Result<()> {
    for entry in walkdir(wiki)? {
        if entry.extension().and_then(|e| e.to_str()) == Some("md") {
            std::fs::remove_file(entry)?;
        }
    }
    Ok(())
}

fn walkdir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            found.extend(walkdir(&path)?);
        } else {
            found.push(path);
        }
    }
    Ok(found)
}

fn write_run_log(
    home: &Path,
    started: &chrono::DateTime<chrono::Local>,
    events: &[Event],
    result: &Result<Outcome>,
    backend: &dyn Backend,
    settings: &model::ModelSettings,
) -> Result<Option<PathBuf>> {
    let dir = home
        .join("state/runs")
        .join(started.format("%Y%m%dT%H%M%S").to_string());
    std::fs::create_dir_all(&dir)?;

    let mut report = String::new();
    report.push_str(&format!("# build {}\n\n", started.to_rfc3339()));
    report.push_str(&format!(
        "actor: {}\nprovider: {}\nmodel: {}\n\n",
        backend.actor(),
        settings.provider,
        settings.name
    ));

    report.push_str(&format!("## events ({})\n\n", events.len()));
    for event in events {
        report.push_str(&format!("- {} {} {:?}\n", event.id, event.ts, event.text()));
    }

    match result {
        Ok(outcome) => {
            report.push_str(&format!(
                "\n## outcome\n\nturns: {}\ninput tokens: {:?}\noutput tokens: {:?}\n\n{}\n",
                outcome.turns,
                outcome.usage.input_tokens,
                outcome.usage.output_tokens,
                outcome.summary
            ));
            report.push_str(&format!(
                "\n## pages written\n\n{}\n",
                if outcome.journal.writes.is_empty() {
                    "(none)".to_string()
                } else {
                    outcome.journal.writes.join("\n")
                }
            ));
            report.push_str("\n## transcript\n\n");
            for line in &outcome.transcript {
                report.push_str(&format!("{line}\n"));
            }
        }
        Err(error) => {
            report.push_str(&format!("\n## FAILED\n\n{error:#}\n"));
        }
    }

    std::fs::write(dir.join("run.md"), report)?;
    std::fs::write(dir.join("system-prompt.md"), crate::reconcile::SYSTEM)?;
    Ok(Some(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips() {
        let dir = crate::tools::tests::tempdir::Dir::new();
        let cursor = Cursor {
            last_event: Some("01K2C57TDA6P1YQ8W3ZR4H2J9C".into()),
            built_at: Some("2026-08-13T21:00:00-04:00".into()),
        };
        cursor.save(dir.path()).unwrap();
        let loaded = Cursor::load(dir.path()).unwrap();
        assert_eq!(loaded.last_event, cursor.last_event);
    }

    #[test]
    fn missing_cursor_means_never_built() {
        let dir = crate::tools::tests::tempdir::Dir::new();
        assert!(Cursor::load(dir.path()).unwrap().last_event.is_none());
    }
}

#[cfg(test)]
mod integration {
    use super::*;
    use crate::model::tests::ScriptedBackend;

    fn write_and_finish(path: &str, content: &str) -> std::sync::Arc<ScriptedBackend> {
        ScriptedBackend::new(vec![
            ScriptedBackend::tool_call(
                "write_page",
                serde_json::json!({"path": path, "content": content}),
            ),
            ScriptedBackend::tool_call("finish", serde_json::json!({"summary": "Recorded it."})),
        ])
    }

    /// A repo with a git identity, ready to build.
    fn repo() -> crate::tools::tests::tempdir::Dir {
        let dir = crate::tools::tests::tempdir::Dir::new();
        let home = dir.path();
        std::fs::create_dir_all(home.join("log")).unwrap();
        std::fs::create_dir_all(home.join("wiki")).unwrap();
        std::fs::create_dir_all(home.join("state")).unwrap();
        crate::git::run(home, &["init", "-q"]).unwrap();
        crate::git::run(home, &["config", "user.email", "test@example.com"]).unwrap();
        crate::git::run(home, &["config", "user.name", "test"]).unwrap();
        std::fs::write(home.join("wiki/index.md"), "# lifewiki\n").unwrap();
        crate::git::commit(home, &["wiki"], "init").unwrap();
        dir
    }

    fn settings() -> model::ModelSettings {
        model::ModelSettings::default()
    }

    #[test]
    fn folds_events_advances_cursor_and_commits() {
        let dir = repo();
        let home = dir.path();
        let log = Log::open(home).unwrap();
        let first = log.append("oranges are my favourite fruit".into()).unwrap();

        let backend = write_and_finish(
            "/opinions/food.md",
            "---\ntype: Preference\n---\n\nPrefers oranges.\n",
        );
        let report = build_with(home, Options::default(), backend, &settings()).unwrap();

        assert_eq!(report.events, 1);
        assert!(report.committed);
        assert!(home.join("wiki/opinions/food.md").exists());

        // Cursor advanced to the folded event.
        let cursor = Cursor::load(home).unwrap();
        assert_eq!(cursor.last_event.as_deref(), Some(first.id.as_str()));

        // The commit covers wiki, cursor and log together, so the repo never
        // records a wiki claiming to reflect events it hasn't seen.
        let files =
            crate::git::capture(home, &["show", "--name-only", "--format=", "HEAD"]).unwrap();
        assert!(files.contains("wiki/opinions/food.md"), "{files}");
        assert!(files.contains("state/cursor.json"), "{files}");
        assert!(files.contains("log/"), "{files}");

        // And a run log exists for auditing.
        assert!(report.run_dir.unwrap().join("run.md").exists());
    }

    #[test]
    fn second_build_with_no_new_events_is_a_no_op() {
        let dir = repo();
        let home = dir.path();
        Log::open(home).unwrap().append("something".into()).unwrap();

        let backend = write_and_finish("/a.md", "---\ntype: Reference\n---\n\nSomething.\n");
        build_with(home, Options::default(), backend, &settings()).unwrap();
        let head = crate::git::capture(home, &["rev-parse", "HEAD"]).unwrap();

        // An exhausted script would error if the agent ran at all.
        let empty = ScriptedBackend::new(vec![]);
        let report = build_with(home, Options::default(), empty, &settings()).unwrap();

        assert_eq!(report.events, 0);
        assert!(!report.committed);
        assert_eq!(
            head,
            crate::git::capture(home, &["rev-parse", "HEAD"]).unwrap()
        );
    }

    #[test]
    fn only_events_after_the_cursor_are_folded() {
        let dir = repo();
        let home = dir.path();
        let log = Log::open(home).unwrap();
        log.append("first".into()).unwrap();

        let backend = write_and_finish("/a.md", "---\ntype: Reference\n---\n\nFirst.\n");
        build_with(home, Options::default(), backend, &settings()).unwrap();

        let second = log.append("second".into()).unwrap();
        let backend = write_and_finish("/b.md", "---\ntype: Reference\n---\n\nSecond.\n");
        let report = build_with(home, Options::default(), backend, &settings()).unwrap();

        assert_eq!(report.events, 1, "only the new event should be folded");
        assert_eq!(
            Cursor::load(home).unwrap().last_event.as_deref(),
            Some(second.id.as_str())
        );
    }

    #[test]
    fn a_failed_run_leaves_the_cursor_alone_and_still_logs() {
        let dir = repo();
        let home = dir.path();
        Log::open(home).unwrap().append("something".into()).unwrap();

        // Never calls finish, so the turn limit trips.
        let backend = ScriptedBackend::new(
            (0..5)
                .map(|_| ScriptedBackend::tool_call("list_pages", serde_json::json!({})))
                .collect(),
        );
        let options = Options {
            max_turns: 2,
            ..Options::default()
        };
        assert!(build_with(home, options, backend, &settings()).is_err());

        assert!(
            Cursor::load(home).unwrap().last_event.is_none(),
            "cursor must not advance past a failed build"
        );
        // The transcript of a failed run is exactly when it's most useful.
        let runs: Vec<_> = std::fs::read_dir(home.join("state/runs"))
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(runs.len(), 1);
        let report = std::fs::read_to_string(runs[0].path().join("run.md")).unwrap();
        assert!(report.contains("FAILED"), "{report}");
    }

    #[test]
    fn refuses_to_run_on_a_dirty_wiki() {
        let dir = repo();
        let home = dir.path();
        Log::open(home).unwrap().append("something".into()).unwrap();
        std::fs::write(home.join("wiki/scratch.md"), "uncommitted\n").unwrap();

        let backend = ScriptedBackend::new(vec![]);
        let error = build_with(home, Options::default(), backend, &settings())
            .unwrap_err()
            .to_string();
        assert!(error.contains("uncommitted"), "{error}");
    }

    #[test]
    fn dry_run_does_not_commit_or_advance_the_cursor() {
        let dir = repo();
        let home = dir.path();
        Log::open(home).unwrap().append("something".into()).unwrap();
        let head = crate::git::capture(home, &["rev-parse", "HEAD"]).unwrap();

        let backend = write_and_finish("/a.md", "---\ntype: Reference\n---\n\nSomething.\n");
        let options = Options {
            dry_run: true,
            ..Options::default()
        };
        let report = build_with(home, options, backend, &settings()).unwrap();

        assert!(!report.committed);
        assert!(Cursor::load(home).unwrap().last_event.is_none());
        assert_eq!(
            head,
            crate::git::capture(home, &["rev-parse", "HEAD"]).unwrap()
        );
    }

    #[test]
    fn into_builds_elsewhere_and_leaves_the_repo_untouched() {
        let dir = repo();
        let scratch = crate::tools::tests::tempdir::Dir::new();
        let home = dir.path();
        Log::open(home).unwrap().append("something".into()).unwrap();
        let head = crate::git::capture(home, &["rev-parse", "HEAD"]).unwrap();

        let backend = write_and_finish("/a.md", "---\ntype: Reference\n---\n\nSomething.\n");
        let options = Options {
            into: Some(scratch.path().to_path_buf()),
            ..Options::default()
        };
        let report = build_with(home, options, backend, &settings()).unwrap();

        assert!(scratch.path().join("a.md").exists());
        assert!(!home.join("wiki/a.md").exists());
        assert!(!report.committed);
        assert!(Cursor::load(home).unwrap().last_event.is_none());
        assert_eq!(
            head,
            crate::git::capture(home, &["rev-parse", "HEAD"]).unwrap()
        );
    }

    #[test]
    fn to_bounds_the_replay() {
        let dir = repo();
        let home = dir.path();
        let log = Log::open(home).unwrap();
        let first = log.append("first".into()).unwrap();
        log.append("second".into()).unwrap();

        let backend = write_and_finish("/a.md", "---\ntype: Reference\n---\n\nFirst only.\n");
        let options = Options {
            to: Some(first.id.clone()),
            ..Options::default()
        };
        let report = build_with(home, options, backend, &settings()).unwrap();
        assert_eq!(report.events, 1);
    }

    #[test]
    fn redacted_events_are_skipped() {
        let dir = repo();
        let home = dir.path();
        let log = Log::open(home).unwrap();
        let secret = log.append("my password is hunter2".into()).unwrap();
        log.redact(&secret.id, "password").unwrap();

        let empty = ScriptedBackend::new(vec![]);
        let report = build_with(home, Options::default(), empty, &settings()).unwrap();
        assert_eq!(report.events, 0, "a redacted event has no text to fold");
    }
}

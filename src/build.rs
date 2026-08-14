//! `lw build` — fold new log events into the wiki.
//!
//! The transactional shape matters more than the model call. A build must be
//! idempotent (nothing new ⇒ no work, no commit), crash-safe (an interrupted
//! run leaves a dirty tree that `git checkout` discards, and the cursor
//! unmoved), and atomic (wiki and cursor move in one commit, so `git revert` of
//! a build is complete).

use crate::event::Event;
use crate::git;
use crate::log::Log;
use crate::model::{self, Backend};
use crate::agent::{Agent, Outcome, BUILD};
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
    /// Write the wiki here instead of `<home>/wiki`, for comparing prompts.
    pub into: Option<PathBuf>,
    /// Start from an empty wiki and replay the whole log.
    pub rebuild: bool,
    /// Override the provider from config.toml, e.g. "local" from `--local`.
    pub provider_override: Option<String>,
    /// Print only the result, without narrating the run.
    pub quiet: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            into: None,
            rebuild: false,
            provider_override: None,
            quiet: false,
        }
    }
}

#[derive(Debug)]
pub struct Report {
    pub events: usize,
    pub outcome: Option<Outcome>,
    pub committed: bool,
}

pub fn build(home: &Path, options: Options) -> Result<Report> {
    let mut settings = model::ModelSettings::load(home, model::Task::Build)?;
    if let Some(provider) = &options.provider_override {
        settings = settings.with_provider(provider);
    }

    // Cheap checks first: a dirty tree or an up-to-date wiki should be
    // reported before we complain about a missing API key.
    preflight(home, &options)?;
    if pending_events(home, &options)?.is_empty() {
        return Ok(Report {
            events: 0,
            outcome: None,
            committed: false,
        });
    }

    let backend = model::build(&settings)?;
    build_with(home, options, backend)
}

/// Refuse to start if the wiki has changes we would mix into our own commit or
/// destroy. The message names the files, because "uncommitted changes" with no
/// list is the kind of error that leaves you running `git status` yourself.
fn preflight(home: &Path, options: &Options) -> Result<()> {
    if options.into.is_some() {
        return Ok(());
    }
    let dirty = git::capture(home, &["status", "--porcelain", "--", "wiki"])?;
    if dirty.trim().is_empty() {
        return Ok(());
    }

    let files: Vec<&str> = dirty.lines().map(str::trim).collect();
    let mut message = String::from("wiki/ has uncommitted changes:\n");
    for file in &files {
        message.push_str(&format!("  {file}\n"));
    }
    message.push_str(&format!(
        "\nA build rewrites these pages, so it will not run while they differ \
         from the last commit.\n\n\
         If the changes are yours and you want to keep them:\n  \
         git -C {home} add wiki && git -C {home} commit -m 'wiki: manual edits'\n\n\
         If you want to discard them:\n  \
         git -C {home} checkout -- wiki/\n",
        home = home.display()
    ));

    // An editor opening the wiki is the common cause, and the fix is a
    // gitignore entry rather than a commit.
    if files.iter().any(|file| file.contains(".obsidian")) {
        message.push_str(
            "\nwiki/.obsidian/ is editor state, not content. `lw init` now ignores \
             it; add it to .gitignore in this repo:\n  \
             echo '.obsidian/' >> .gitignore\n",
        );
    }
    bail!("{message}");
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
            committed: false,
        });
    }

    if options.rebuild && wiki.exists() {
        clear_markdown(&wiki)?;
    }

    let mut workspace = Workspace::new(&wiki)?;
    workspace.writable = true;

    let reporter = std::sync::Arc::new(if options.quiet {
        crate::progress::Reporter::silent()
    } else {
        crate::progress::Reporter::new()
    });
    let mut agent = Agent::new(backend.clone(), BUILD);
    agent.reporter = reporter.clone();

    let started = chrono::Local::now();
    reporter.started(events.len(), &agent.actor());
    let opening = crate::prompts::build_opening(
        &events,
        &agent.actor(),
        &started.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    let result = agent.run(workspace, &opening).map(|(outcome, _)| outcome);

    // A rebuild clears the wiki before the agent runs, so a failure would
    // otherwise leave it empty — the pages are recoverable from git, but only
    // if you know to look. Restore them ourselves instead.
    if result.is_err() && options.rebuild && target_is_default {
        match git::run(home, &["checkout", "--", "wiki/"]) {
            Ok(()) => eprintln!("\nRebuild failed; restored wiki/ from the last commit."),
            Err(error) => eprintln!(
                "\nRebuild failed and wiki/ could not be restored: {error:#}\n\
                 Recover it with: git -C {} checkout -- wiki/",
                home.display()
            ),
        }
    }

    let outcome = result?;

    // The index is a mechanical listing of frontmatter that already exists, so
    // it is generated rather than left to the model.
    let indexed = crate::index::write(&wiki).context("could not write wiki/index.md")?;
    reporter.note(&format!("index.md lists {indexed} pages"));

    // Reported, not fatal: the wiki is more useful with a few broken links than
    // not written at all, and the agent may simply not have made a page yet.
    for dangling in crate::index::dangling_links(&wiki)? {
        reporter.warn(&format!(
            "warning: {} links to {}, which does not exist",
            dangling.from, dangling.to
        ));
    }

    let last = events.last().expect("events is non-empty").id.clone();

    // Only the default target participates in the repo's history; `--into` is
    // a scratch materialisation for comparing prompts.
    if !target_is_default {
        return Ok(Report {
            events: events.len(),
            outcome: Some(outcome),
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
        outcome.result
    );

    // Wiki and cursor in one commit, so the repo never records a wiki claiming
    // to reflect events it hasn't seen.
    git::commit(home, &["wiki", "state/cursor.json"], &message)?;

    Ok(Report {
        events: events.len(),
        outcome: Some(outcome),
        committed: true,
    })
}

/// Remove every page, so a rebuild starts from nothing.
fn clear_markdown(wiki: &Path) -> Result<()> {
    for entry in crate::okf::pages(wiki)? {
        if entry.extension().and_then(|e| e.to_str()) == Some("md") {
            std::fs::remove_file(entry)?;
        }
    }
    Ok(())
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
        let report = build_with(home, Options::default(), backend).unwrap();

        assert_eq!(report.events, 1);
        assert!(report.committed);
        assert!(home.join("wiki/opinions/food.md").exists());

        // Cursor advanced to the folded event.
        let cursor = Cursor::load(home).unwrap();
        assert_eq!(cursor.last_event.as_deref(), Some(first.id.as_str()));

        // Wiki and cursor move together, so the repo never records a wiki
        // claiming to reflect events it hasn't seen.
        let files =
            crate::git::capture(home, &["show", "--name-only", "--format=", "HEAD"]).unwrap();
        assert!(files.contains("wiki/opinions/food.md"), "{files}");
        assert!(files.contains("state/cursor.json"), "{files}");
    }

    #[test]
    fn second_build_with_no_new_events_is_a_no_op() {
        let dir = repo();
        let home = dir.path();
        Log::open(home).unwrap().append("something".into()).unwrap();

        let backend = write_and_finish("/a.md", "---\ntype: Reference\n---\n\nSomething.\n");
        build_with(home, Options::default(), backend).unwrap();
        let head = crate::git::capture(home, &["rev-parse", "HEAD"]).unwrap();

        // An exhausted script would error if the agent ran at all.
        let empty = ScriptedBackend::new(vec![]);
        let report = build_with(home, Options::default(), empty).unwrap();

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
        build_with(home, Options::default(), backend).unwrap();

        let second = log.append("second".into()).unwrap();
        let backend = write_and_finish("/b.md", "---\ntype: Reference\n---\n\nSecond.\n");
        let report = build_with(home, Options::default(), backend).unwrap();

        assert_eq!(report.events, 1, "only the new event should be folded");
        assert_eq!(
            Cursor::load(home).unwrap().last_event.as_deref(),
            Some(second.id.as_str())
        );
    }

    #[test]
    fn a_failed_run_leaves_the_cursor_alone() {
        let dir = repo();
        let home = dir.path();
        Log::open(home).unwrap().append("something".into()).unwrap();

        // Never calls finish, so the turn limit trips.
        let backend = ScriptedBackend::new(
            (0..5)
                .map(|_| ScriptedBackend::tool_call("list_pages", serde_json::json!({})))
                .collect(),
        );
        assert!(build_with(home, Options::default(), backend).is_err());

        assert!(
            Cursor::load(home).unwrap().last_event.is_none(),
            "cursor must not advance past a failed build"
        );
    }

    /// A rebuild clears the wiki before the agent runs, so a mid-run failure
    /// must not leave the user with an empty wiki.
    #[test]
    fn a_failed_rebuild_restores_the_wiki() {
        let dir = repo();
        let home = dir.path();
        Log::open(home).unwrap().append("something".into()).unwrap();
        std::fs::create_dir_all(home.join("wiki/people")).unwrap();
        std::fs::write(
            home.join("wiki/people/ada.md"),
            "---\ntype: Person\n---\n\nAda.\n",
        )
        .unwrap();
        crate::git::commit(home, &["wiki"], "seed").unwrap();

        // Exhausted script: the agent fails on its first inference.
        let backend = ScriptedBackend::new(vec![]);
        let options = Options {
            rebuild: true,
            ..Options::default()
        };
        assert!(build_with(home, options, backend).is_err());

        assert!(
            home.join("wiki/people/ada.md").exists(),
            "a failed rebuild left the wiki empty"
        );
        assert!(
            crate::git::capture(home, &["status", "--porcelain", "--", "wiki"])
                .unwrap()
                .trim()
                .is_empty(),
            "wiki should be back to its committed state"
        );
    }

    #[test]
    fn refuses_to_run_on_a_dirty_wiki() {
        let dir = repo();
        let home = dir.path();
        Log::open(home).unwrap().append("something".into()).unwrap();
        std::fs::write(home.join("wiki/scratch.md"), "uncommitted\n").unwrap();

        let backend = ScriptedBackend::new(vec![]);
        let error = build_with(home, Options::default(), backend)
            .unwrap_err()
            .to_string();
        assert!(error.contains("uncommitted"), "{error}");
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
        let report = build_with(home, options, backend).unwrap();

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
    fn redacted_events_are_skipped() {
        let dir = repo();
        let home = dir.path();
        let log = Log::open(home).unwrap();
        let secret = log.append("my password is hunter2".into()).unwrap();
        log.redact(&secret.id, "password").unwrap();

        let empty = ScriptedBackend::new(vec![]);
        let report = build_with(home, Options::default(), empty).unwrap();
        assert_eq!(report.events, 0, "a redacted event has no text to fold");
    }
}

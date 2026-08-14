use crate::{log::Log, paths};
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::{
    env, fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Parser)]
#[command(name = "lw", about = "A local-first personal event log")]
struct Cli {
    #[command(subcommand)]
    command: CommandName,
}
#[derive(Subcommand)]
enum CommandName {
    Init,
    Add {
        #[arg(trailing_var_arg = true)]
        text: Vec<String>,
    },
    Log {
        #[arg(short = 'n', default_value_t = 20)]
        number: usize,
    },
    Show {
        id: String,
    },
    Search {
        query: String,
    },
    Status,
    Sync,
    Doctor {
        #[arg(long)]
        repair: bool,
    },
    Redact {
        id: String,
        #[arg(long)]
        reason: String,
    },
    /// Fold new log events into the wiki.
    Build {
        /// Write the wiki here instead of <home>/wiki, for comparing prompts.
        #[arg(long, value_name = "DIR")]
        into: Option<PathBuf>,
        /// Stop after this event id (inclusive).
        #[arg(long, value_name = "ID")]
        to: Option<String>,
        /// Run the agent but discard the result and skip the commit.
        #[arg(long)]
        dry_run: bool,
        /// Maximum agent turns before the run fails.
        #[arg(long, default_value_t = crate::reconcile::DEFAULT_MAX_TURNS)]
        max_turns: usize,
    },
    /// Rebuild the whole wiki from the log, discarding the current one.
    Rebuild {
        #[arg(long, value_name = "DIR")]
        into: Option<PathBuf>,
        #[arg(long, value_name = "ID")]
        to: Option<String>,
        #[arg(long, default_value_t = crate::reconcile::DEFAULT_MAX_TURNS)]
        max_turns: usize,
        /// Required unless --into is given, since this discards the wiki.
        #[arg(long)]
        force: bool,
    },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let home = paths::home()?;
    match cli.command {
        CommandName::Init => init(&home),
        CommandName::Add { text } => {
            paths::require_initialized(&home)?;
            let text = capture_text(text)?;
            let event = Log::open(&home)?.append(text)?;
            println!("{}", event.id);
            Ok(())
        }
        CommandName::Log { number } => {
            paths::require_initialized(&home)?;
            for event in Log::open(&home)?.events()?.into_iter().rev().take(number) {
                println!(
                    "{}  {}  {}",
                    event.id,
                    event.ts,
                    event.one_line()
                );
            }
            Ok(())
        }
        CommandName::Show { id } => {
            paths::require_initialized(&home)?;
            let event = Log::open(&home)?.resolve_prefix(&id)?;
            println!("{}\n{}\n{}", event.id, event.ts, event.text());
            Ok(())
        }
        CommandName::Search { query } => {
            paths::require_initialized(&home)?;
            let regex = regex(&query)?;
            for event in Log::open(&home)?.events()? {
                if regex.is_match(event.text()) {
                    println!(
                        "{}  {}  {}",
                        event.id,
                        event.ts,
                        event.one_line()
                    );
                }
            }
            Ok(())
        }
        CommandName::Status => status(&home),
        CommandName::Sync => sync(&home),
        CommandName::Doctor { repair } => {
            paths::require_initialized(&home)?;
            let problems = Log::open(&home)?.check(repair)?;
            if problems.is_empty() {
                println!("log is healthy");
            } else {
                for p in problems {
                    println!("{p}");
                }
            }
            Ok(())
        }
        CommandName::Build {
            into,
            to,
            dry_run,
            max_turns,
        } => {
            paths::require_initialized(&home)?;
            run_build(
                &home,
                crate::build::Options {
                    into,
                    to,
                    rebuild: false,
                    dry_run,
                    max_turns,
                },
            )
        }
        CommandName::Rebuild {
            into,
            to,
            max_turns,
            force,
        } => {
            paths::require_initialized(&home)?;
            if into.is_none() && !force {
                bail!(
                    "rebuild discards the current wiki and re-derives it from the log.\n\
                     Re-run with --force, or use --into <dir> to build a copy elsewhere."
                );
            }
            run_build(
                &home,
                crate::build::Options {
                    into,
                    to,
                    rebuild: true,
                    dry_run: false,
                    max_turns,
                },
            )
        }
        CommandName::Redact { id, reason } => {
            paths::require_initialized(&home)?;
            let event = Log::open(&home)?.redact(&id, &reason)?;
            println!("Redacted {}. The original text remains in git history; use git filter-repo to remove it fully.", event.id);
            Ok(())
        }
    }
}

fn init(home: &Path) -> Result<()> {
    fs::create_dir_all(home.join("log"))?;
    fs::create_dir_all(home.join("state/runs"))?;
    fs::create_dir_all(home.join("wiki"))?;
    if !home.join("config.toml").exists() {
        fs::write(
            home.join("config.toml"),
            concat!(
                "# lifewiki configuration\n",
                "# pending_warning_hours = 24\n\n",
                "[model]\n",
                "provider = \"anthropic\"   # anthropic | none\n",
                "name = \"claude-sonnet-4-5\"\n",
                "timeout_seconds = 120\n",
            ),
        )?;
    }
    if !home.join("wiki/index.md").exists() {
        fs::write(home.join("wiki/index.md"), "# lifewiki\n")?;
    }
    if !home.join(".git").exists() {
        git(home, &["init"])?;
        git(home, &["add", "config.toml", "wiki"])?;
        git(home, &["commit", "-m", "lifewiki: initialize"])?;
    }
    println!("Initialized lifewiki at {}", home.display());
    Ok(())
}
fn capture_text(args: Vec<String>) -> Result<String> {
    if !args.is_empty() {
        return Ok(args.join(" "));
    }
    if !atty_stdin() {
        let mut text = String::new();
        io::stdin().read_to_string(&mut text)?;
        return Ok(text);
    }
    let editor = env::var("EDITOR")
        .context("no text provided, stdin is a terminal, and EDITOR is not set")?;
    let path = env::temp_dir().join(format!("lifewiki-{}.txt", std::process::id()));
    fs::write(&path, "")?;
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("{} '{}'", editor, path.display()))
        .status()?;
    if !status.success() {
        bail!("editor exited unsuccessfully");
    }
    let text = fs::read_to_string(&path)?;
    let _ = fs::remove_file(path);
    Ok(text)
}
fn atty_stdin() -> bool {
    unsafe { libc::isatty(0) == 1 }
}
fn regex(query: &str) -> Result<regex::Regex> {
    regex::Regex::new(query).context("invalid regular expression")
}
fn git(home: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .current_dir(home)
        .status()
        .context("failed to run git")?;
    if status.success() {
        Ok(())
    } else {
        bail!(
            "git {} failed (configure user.name and user.email, then retry)",
            args.join(" ")
        )
    }
}
fn status(home: &Path) -> Result<()> {
    paths::require_initialized(home)?;
    let log = Log::open(home)?;
    let cursor = crate::build::Cursor::load(home)?;

    let unfolded = match &cursor.last_event {
        Some(id) => log.since(id)?,
        None => log.events()?,
    };
    match &cursor.built_at {
        Some(at) => println!("Last build: {at}"),
        None => println!("Last build: never"),
    }
    println!(
        "{} event{} not yet folded into the wiki",
        unfolded.len(),
        if unfolded.len() == 1 { "" } else { "s" }
    );

    // Uncommitted log events exist only in the working tree, so they are not
    // backed up by a push (S12).
    let uncommitted = crate::git::capture(home, &["status", "--porcelain", "--", "log"])?;
    if !uncommitted.trim().is_empty() {
        let pending = log.events()?;
        let oldest = pending
            .iter()
            .rev()
            .filter_map(|event| chrono::DateTime::parse_from_rfc3339(&event.ts).ok())
            .last();
        print!("Log has uncommitted events");
        if let Some(oldest) = oldest {
            let age = chrono::Local::now().signed_duration_since(oldest);
            print!(" (oldest {} hours old)", age.num_hours());
            if age.num_hours() >= 24 {
                print!(" — run `lw sync` to back them up");
            }
        }
        println!();
    }

    if crate::git::is_dirty(home, "wiki")? {
        println!("wiki/ has uncommitted changes; `lw build` will refuse to run");
    }
    Ok(())
}

fn sync(home: &Path) -> Result<()> {
    paths::require_initialized(home)?;
    let status = Command::new("git")
        .args(["status", "--porcelain", "--", "log"])
        .current_dir(home)
        .output()?;
    if status.stdout.is_empty() {
        println!("No pending log events.");
        return Ok(());
    }
    git(home, &["add", "log"])?;
    git(home, &["commit", "-m", "log: captured events"])?;
    let push = Command::new("git").arg("push").current_dir(home).status()?;
    if !push.success() {
        bail!("events were committed, but git push failed");
    }
    println!("Synced pending log events.");
    Ok(())
}

fn run_build(home: &Path, options: crate::build::Options) -> Result<()> {
    let dry_run = options.dry_run;
    let report = crate::build::build(home, options)?;

    if report.events == 0 {
        println!("Nothing to build; the wiki is up to date.");
        return Ok(());
    }

    if let Some(outcome) = &report.outcome {
        println!(
            "Folded {} event{} in {} turn{}.",
            report.events,
            if report.events == 1 { "" } else { "s" },
            outcome.turns,
            if outcome.turns == 1 { "" } else { "s" }
        );
        if !outcome.journal.writes.is_empty() {
            for page in &outcome.journal.writes {
                println!("  {page}");
            }
        }
        if !outcome.summary.is_empty() {
            println!("\n{}", outcome.summary);
        }
    }
    if let Some(dir) = &report.run_dir {
        println!("\nRun log: {}", dir.display());
    }
    if dry_run {
        println!("\nDry run: changes were written to the working tree but not committed.");
        println!("Discard them with `git -C {} checkout -- wiki/`", home.display());
    } else if !report.committed {
        println!("\nNot committed.");
    }
    Ok(())
}

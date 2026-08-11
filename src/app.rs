use crate::{log::Log, paths};
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::{
    env, fs,
    io::{self, Read},
    path::Path,
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
                    event.text.replace('\n', "\\n")
                );
            }
            Ok(())
        }
        CommandName::Show { id } => {
            paths::require_initialized(&home)?;
            let event = Log::open(&home)?.resolve_prefix(&id)?;
            println!("{}\n{}\n{}", event.id, event.ts, event.text);
            Ok(())
        }
        CommandName::Search { query } => {
            paths::require_initialized(&home)?;
            let regex = regex(&query)?;
            for event in Log::open(&home)?.events()? {
                if regex.is_match(&event.text) {
                    println!(
                        "{}  {}  {}",
                        event.id,
                        event.ts,
                        event.text.replace('\n', "\\n")
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
            "# lifewiki configuration\n# pending_warning_hours = 24\n",
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
    let output = Command::new("git")
        .args(["status", "--porcelain", "--", "log"])
        .current_dir(home)
        .output()?;
    let dirty = !output.stdout.is_empty();
    let count = if dirty {
        Log::open(home)?.events()?.len()
    } else {
        0
    };
    println!(
        "{} pending event{}",
        count,
        if count == 1 { "" } else { "s" }
    );
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

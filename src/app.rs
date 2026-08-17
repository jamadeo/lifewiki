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
#[command(
    name = "lw",
    about = "A local-first personal event log that builds itself into a wiki",
    long_about = "lifewiki records what you tell it about your life to an \
append-only log, then uses a language model to fold those notes into a \
version-controlled wiki.\n\n\
The log is the source of truth and is never edited. The wiki is derived, so it \
can always be thrown away and rebuilt from the log.",
    after_help = "Run `lw <command> --help` for details on any command.",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: CommandName,
}

#[derive(Subcommand)]
enum CommandName {
    /// Create a new lifewiki repository.
    ///
    /// Creates log/, wiki/ and state/ under ~/.lifewiki (or $LIFEWIKI_HOME),
    /// writes a default config.toml and .gitignore, and makes the first git
    /// commit. Safe to re-run: existing files are left alone.
    Init,

    /// Record something in the log.
    ///
    /// Committed to git immediately, so an entry is backed up as soon as it
    /// exists.
    ///
    /// The text is stored verbatim and never edited afterwards. Write in plain
    /// prose; there are no tags, types or categories to choose. To correct or
    /// update something, just say so in a new entry — the wiki build works out
    /// what changed:
    ///
    ///   lw add "Ada's birthday is the 14th, not the 4th"
    ///
    /// With no arguments, reads stdin if it is piped, otherwise opens $EDITOR.
    /// However the text arrives, one invocation records exactly one event, so
    /// blank lines are kept rather than splitting it up.
    #[command(visible_alias = "a")]
    Add {
        /// The text to record. Quoting is optional.
        #[arg(trailing_var_arg = true, value_name = "TEXT")]
        text: Vec<String>,
    },

    /// List recent entries, newest first.
    #[command(visible_alias = "ls")]
    Log {
        /// How many entries to show.
        #[arg(short = 'n', default_value_t = 20, value_name = "COUNT")]
        number: usize,
    },

    /// Show one entry in full.
    ///
    /// The id may be abbreviated to any unique prefix, like a git hash. An
    /// ambiguous prefix lists the candidates rather than guessing.
    Show {
        /// Event id, or a unique prefix of one.
        #[arg(value_name = "ID")]
        id: String,
    },

    /// Search the log with a regular expression.
    ///
    /// Matching is case-sensitive; prefix the pattern with `(?i)` to ignore
    /// case. This searches the log, not the wiki — it finds what you wrote,
    /// rather than what was derived from it.
    #[command(visible_alias = "s")]
    Search {
        /// Regular expression to match against entry text.
        #[arg(value_name = "PATTERN")]
        query: String,
    },

    /// Show how far the wiki lags the log, and which models are configured.
    Status,

    /// Check the log and wiki for damage.
    ///
    /// For the log: that every line in every month file parses as a valid
    /// entry. A crash mid-write can leave a torn final line; that is the case
    /// this is built to find.
    ///
    /// For the wiki: that every page still conforms to OKF, and that internal
    /// links resolve. Pages are validated when written, but a hand-edit or a
    /// deleted page can break them afterwards.
    ///
    /// Reports problems; it never changes anything.
    Doctor,

    /// Permanently erase the text of one entry.
    ///
    /// The documented exception to the log being append-only, for when a
    /// password or someone else's secret gets pasted in. The entry's id and
    /// timestamp remain, with the text replaced by a tombstone recording your
    /// reason, so nothing that referenced it dangles.
    ///
    /// Note that git keeps the old text in history. To erase it there too you
    /// will need `git filter-repo`; see docs/event-log.md.
    Redact {
        /// Event id, or a unique prefix of one.
        #[arg(value_name = "ID")]
        id: String,
        /// Why the text was erased. Recorded in place of the text.
        #[arg(long, value_name = "REASON")]
        reason: String,
    },

    /// Fold new log entries into the wiki.
    ///
    /// Runs an agent that searches the wiki, reads whatever the new entries
    /// might affect, and rewrites those pages. Its main job is reconciliation:
    /// noticing that "I prefer apples now" contradicts a page saying oranges,
    /// and fixing it rather than adding a second, contradictory page.
    ///
    /// Only entries added since the last build are folded. The wiki, the
    /// cursor and the log move in a single commit, so `git revert` of that
    /// commit undoes the whole build.
    ///
    /// Requires a clean wiki/, since a build rewrites pages and would
    /// otherwise mix your edits into its own commit.
    #[command(visible_alias = "b")]
    Build {
        /// Write the wiki here instead of <home>/wiki.
        ///
        /// Leaves the real wiki and the cursor untouched, so two prompt
        /// versions can be built and compared with `diff -r`.
        #[arg(long, value_name = "DIR")]
        into: Option<PathBuf>,
        /// Run inference locally instead of using the configured provider.
        #[cfg(feature = "local")]
        #[arg(long)]
        local: bool,
        /// Load the local model in this process instead of using the daemon.
        #[cfg(feature = "local")]
        #[arg(long)]
        no_daemon: bool,
        /// Print only the result, without narrating the run.
        #[arg(short, long)]
        quiet: bool,
    },

    /// Ask a question about your life, answered from the wiki.
    ///
    /// Searches the wiki, reads what looks relevant, and answers in prose,
    /// citing the pages used. Read-only: a question cannot change the wiki.
    ///
    ///   lw ask "when did I start running again?"
    ///
    /// Answers come only from the wiki, so anything you have not recorded is
    /// unknown to it. It is instructed to say so rather than guess.
    #[command(visible_alias = "q")]
    Ask {
        /// The question. Quoting is optional.
        #[arg(trailing_var_arg = true, value_name = "QUESTION")]
        question: Vec<String>,
        /// Run inference locally instead of using the configured provider.
        #[cfg(feature = "local")]
        #[arg(long)]
        local: bool,
        /// Load the local model in this process instead of using the daemon.
        #[cfg(feature = "local")]
        #[arg(long)]
        no_daemon: bool,
        /// Print only the answer, without the searching and reading.
        #[arg(short, long)]
        quiet: bool,
    },

    /// Discard the wiki and derive it again from the whole log.
    ///
    /// Use this after improving the prompt: the log is the source of truth, so
    /// the past can be re-derived. Every entry is folded again, which on a
    /// hosted provider costs proportionally to your whole history.
    ///
    /// Prefer --into when iterating, so a bad run cannot touch the real wiki:
    /// `lw rebuild --into /tmp/wiki-v2 && diff -r wiki /tmp/wiki-v2`.
    #[command(visible_alias = "rb")]
    Rebuild {
        /// Build into this directory instead, leaving the real wiki alone.
        #[arg(long, value_name = "DIR")]
        into: Option<PathBuf>,
        /// Confirm discarding the current wiki. Required unless --into is given.
        #[arg(long)]
        force: bool,
        /// Run inference locally instead of using the configured provider.
        #[cfg(feature = "local")]
        #[arg(long)]
        local: bool,
        /// Load the local model in this process instead of using the daemon.
        #[cfg(feature = "local")]
        #[arg(long)]
        no_daemon: bool,
        /// Print only the result, without narrating the run.
        #[arg(short, long)]
        quiet: bool,
    },

    /// Manage the local inference daemon.
    ///
    /// Loading a local model takes about a minute, and the loaded model dies
    /// with the process. A daemon holds it so only the first command pays that
    /// cost. It starts on demand; these commands are for inspecting it.
    #[cfg(feature = "local")]
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },

    /// Run the daemon in the foreground. Started for you; not for direct use.
    #[cfg(feature = "local")]
    #[command(name = "__daemon", hide = true)]
    DaemonServe,
}

#[cfg(feature = "local")]
#[derive(Subcommand)]
pub enum DaemonCommand {
    /// Show whether a daemon is running, and what it is holding.
    Status,
    /// Start a daemon without running a command.
    Start,
    /// Stop the daemon, freeing the memory its models occupy.
    Stop,
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
            // Commit immediately: an entry that exists only in the working tree
            // survives a crash but not a lost machine.
            crate::git::commit(&home, &["log"], &format!("log: {}", event.one_line()))?;
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
        CommandName::Doctor => {
            paths::require_initialized(&home)?;
            doctor(&home)
        }
        CommandName::Build {
            into,
            #[cfg(feature = "local")]
            local,
            #[cfg(feature = "local")]
            no_daemon,
            quiet,
        } => {
            paths::require_initialized(&home)?;
            run_build(
                &home,
                crate::build::Options {
                    into,
                    rebuild: false,
                    quiet,
                    local_mode: local_mode(
                        #[cfg(feature = "local")]
                        no_daemon,
                    ),
                    #[cfg(feature = "local")]
                    provider_override: local.then(|| "local".to_string()),
                    #[cfg(not(feature = "local"))]
                    provider_override: None,
                },
            )
        }
        CommandName::Ask {
            question,
            #[cfg(feature = "local")]
            local,
            #[cfg(feature = "local")]
            no_daemon,
            quiet,
        } => {
            paths::require_initialized(&home)?;
            #[cfg(feature = "local")]
            let provider_override = local.then(|| "local".to_string());
            #[cfg(not(feature = "local"))]
            let provider_override: Option<String> = None;
            ask(
                &home,
                question,
                provider_override,
                quiet,
                local_mode(
                    #[cfg(feature = "local")]
                    no_daemon,
                ),
            )
        }

        #[cfg(feature = "local")]
        CommandName::DaemonServe => crate::daemon::serve(&home),

        #[cfg(feature = "local")]
        CommandName::Daemon { command } => daemon_command(&home, command),
        CommandName::Rebuild {
            into,
            force,
            #[cfg(feature = "local")]
            local,
            #[cfg(feature = "local")]
            no_daemon,
            quiet,
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
                    rebuild: true,
                    quiet,
                    local_mode: local_mode(
                        #[cfg(feature = "local")]
                        no_daemon,
                    ),
                    #[cfg(feature = "local")]
                    provider_override: local.then(|| "local".to_string()),
                    #[cfg(not(feature = "local"))]
                    provider_override: None,
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
    fs::create_dir_all(home.join("wiki"))?;
    if !home.join("config.toml").exists() {
        fs::write(
            home.join("config.toml"),
            concat!(
                "# lifewiki configuration\n",
                "#\n",
                "# Each task has its own model. Reconciliation rewrites files over\n",
                "# a long run and rewards a stronger model; answering is short and\n",
                "# read-only, so it is the place to spend less, or to keep local.\n",
                "#\n",
                "# provider: anthropic | local | none\n",
                "#   `local` runs offline and needs a binary built with\n",
                "#   --features local (or metal/cuda/vulkan).\n",
                "# name: omit for the provider's own default.\n",
                "\n",
                "[build]\n",
                "provider = \"anthropic\"\n",
                "name = \"claude-sonnet-4-5\"\n",
                "\n",
                "[ask]\n",
                "provider = \"anthropic\"\n",
                "name = \"claude-sonnet-4-5\"\n",
            ),
        )?;
    }
    if !home.join("wiki/index.md").exists() {
        fs::write(home.join("wiki/index.md"), "# lifewiki\n")?;
    }
    if !home.join(".gitignore").exists() {
        fs::write(home.join(".gitignore"), GITIGNORE)?;
    }
    if !home.join(".git").exists() {
        git(home, &["init"])?;
        git(home, &["add", ".gitignore", "config.toml", "wiki"])?;
        git(home, &["commit", "-m", "lifewiki: initialize"])?;
    }
    println!("Initialized lifewiki at {}", home.display());
    Ok(())
}
/// Untracked files that would otherwise make the tree permanently dirty and
/// block every build.
const GITIGNORE: &str = "\
# Transient: the append lock and the daemon's socket and lock.
.lock
daemon.sock
daemon.lock

# Editor and viewer state. Obsidian writes wiki/.obsidian when a vault is
# opened, which is not part of the wiki's content.
.obsidian/
.DS_Store
";

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


    if crate::git::is_dirty(home, "wiki")? {
        println!("wiki/ has uncommitted changes; `lw build` will refuse to run");
    }
    show_models(home)?;
    Ok(())
}


fn run_build(home: &Path, options: crate::build::Options) -> Result<()> {
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
        if !outcome.result.is_empty() {
            println!("\n{}", outcome.result);
        }
    }
    if !report.committed {
        println!("\nNot committed.");
    }
    Ok(())
}

/// Report which model each task will use. Per-task config is exactly the kind
/// of thing that silently does nothing when mistyped, so make it inspectable.
/// Whether local inference should go through the daemon.
fn local_mode(#[cfg(feature = "local")] no_daemon: bool) -> crate::model::LocalMode {
    #[cfg(feature = "local")]
    {
        if no_daemon {
            crate::model::LocalMode::InProcess
        } else {
            crate::model::LocalMode::Daemon
        }
    }
    #[cfg(not(feature = "local"))]
    crate::model::LocalMode::InProcess
}

#[cfg(feature = "local")]
fn daemon_command(home: &Path, command: DaemonCommand) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    match command {
        DaemonCommand::Status => match runtime.block_on(crate::daemon::probe(home)) {
            Some(status) => {
                println!("Running (pid {})", status.pid);
                println!("Version:  {}", status.binary_version);
                println!("Started:  {}", status.started_at);
                println!("Served:   {} request(s)", status.requests_served);
                if status.models.is_empty() {
                    println!("Models:   none loaded yet");
                } else {
                    println!("Models:   {}", status.models.join(", "));
                }
                if status.binary_version != crate::daemon::BINARY_VERSION {
                    println!(
                        "\nThis binary is {}; the daemon will be restarted on next use.",
                        crate::daemon::BINARY_VERSION
                    );
                }
                Ok(())
            }
            None => {
                println!("Not running.");
                Ok(())
            }
        },
        DaemonCommand::Start => {
            if let Some(status) = runtime.block_on(crate::daemon::probe(home)) {
                println!("Already running (pid {}).", status.pid);
                return Ok(());
            }
            let status = runtime.block_on(crate::daemon::start(home))?;
            println!("Started (pid {}).", status.pid);
            println!("The model loads on the first request.");
            Ok(())
        }
        DaemonCommand::Stop => {
            if runtime.block_on(crate::daemon::stop(home))? {
                println!("Stopped.");
            } else {
                println!("Not running.");
            }
            Ok(())
        }
    }
}

fn show_models(home: &Path) -> Result<()> {
    use crate::model::{ModelSettings, Task};
    let build = ModelSettings::load(home, Task::Build)?;
    let ask = ModelSettings::load(home, Task::Ask)?;
    if build == ask {
        println!("Model: {}", build.describe());
    } else {
        println!("Model (build): {}", build.describe());
        println!("Model (ask):   {}", ask.describe());
    }
    Ok(())
}

fn ask(
    home: &Path,
    question: Vec<String>,
    provider_override: Option<String>,
    quiet: bool,
    local_mode: crate::model::LocalMode,
) -> Result<()> {
    let question = capture_text(question)?;
    if question.trim().is_empty() {
        bail!("no question given");
    }

    let wiki = home.join("wiki");
    // A wiki with only the index has nothing to answer from, and "not in the
    // wiki" would be a misleading answer when the real problem is no build.
    let pages = crate::okf::pages(&wiki)?
        .into_iter()
        .filter(|path| {
            path.file_name().map(|name| name != crate::index::FILENAME).unwrap_or(true)
        })
        .count();
    if pages == 0 {
        bail!(
            "the wiki has no pages yet, so there is nothing to answer from.\n\
             Run `lw build` first."
        );
    }

    let mut settings = crate::model::ModelSettings::load(home, crate::model::Task::Ask)?;
    if let Some(provider) = &provider_override {
        settings = settings.with_provider(provider);
    }
    let backend = crate::model::build(&settings, home, local_mode)?;

    let mut agent = crate::agent::Agent::new(backend, crate::agent::ASK);
    if !quiet {
        agent.reporter = std::sync::Arc::new(crate::progress::Reporter::new());
    }

    let mut workspace = crate::tools::Workspace::new(&wiki)?;
    workspace.writable = false;
    workspace.home = Some(home.to_path_buf());

    let opening = crate::prompts::ask_opening(&question);
    let (outcome, _) = agent.run(workspace, &opening).map_err(|error| {
        // A small local model can spiral rather than concluding a fact is
        // absent, and exhausting the context is how that presents. The raw
        // provider message doesn't hint at the actual cause.
        // `{:#}` walks the cause chain: to_string() shows only the outermost
        // context, which hides the provider's actual complaint.
        if format!("{error:#}").contains("Context length exceeded") {
            anyhow::anyhow!(
                "the model ran out of context without reaching an answer.\n\
                 This usually means the wiki does not cover the question, and the \
                 model kept looking instead of saying so.\n\
                 Small local models are especially prone to it; try rephrasing, or \
                 use a hosted provider for this question."
            )
        } else {
            error
        }
    })?;
    println!("{}", outcome.result.trim());
    if !quiet {
        if !outcome.journal.reads.is_empty() {
            eprintln!("\nRead: {}", outcome.journal.reads.join(", "));
        }
        eprintln!(
            "{} turn{}, {} in / {} out tokens",
            outcome.turns,
            if outcome.turns == 1 { "" } else { "s" },
            outcome.usage.input_tokens.unwrap_or(0),
            outcome.usage.output_tokens.unwrap_or(0)
        );
    }
    Ok(())
}

fn doctor(home: &Path) -> Result<()> {
    let mut problems = Log::open(home)?.check()?;
    if problems.is_empty() {
        println!("log is healthy");
    } else {
        for problem in &problems {
            println!("{problem}");
        }
    }

    // Pages are validated when written, but a hand-edit or a deleted page can
    // break them afterwards, so the same gate runs again here.
    let wiki = home.join("wiki");
    let mut wiki_problems = Vec::new();
    for path in crate::okf::pages(&wiki)? {
        let relative = path.strip_prefix(&wiki).unwrap_or(&path).to_string_lossy();
        if relative == crate::index::FILENAME {
            // The index has no frontmatter by design (OKF SPEC §8).
            continue;
        }
        let raw = fs::read_to_string(&path)?;
        if let Err(error) = crate::okf::validate(&raw) {
            wiki_problems.push(format!("wiki/{relative}: {error}"));
        }
    }
    for dangling in crate::index::dangling_links(&wiki)? {
        wiki_problems.push(format!(
            "wiki{} links to {}, which does not exist",
            dangling.from, dangling.to
        ));
    }

    if wiki_problems.is_empty() {
        println!("wiki is healthy");
    } else {
        for problem in &wiki_problems {
            println!("{problem}");
        }
    }

    problems.extend(wiki_problems);
    if !problems.is_empty() {
        println!("\n{} problem(s) found.", problems.len());
    }
    Ok(())
}



#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// Help text is documentation, and documentation that only exists in one
    /// place goes stale silently. This at least enforces that it exists.
    #[test]
    fn every_command_and_argument_is_documented() {
        let command = Cli::command();
        for subcommand in command.get_subcommands() {
            let name = subcommand.get_name();
            if name == "help" {
                continue;
            }
            assert!(
                subcommand.get_about().is_some(),
                "`lw {name}` has no help text"
            );
            for argument in subcommand.get_arguments() {
                let id = argument.get_id();
                if id == "help" {
                    continue;
                }
                assert!(
                    argument.get_help().is_some(),
                    "`lw {name} --{id}` has no help text"
                );
            }
        }
    }

    /// The top-level `--help` is the first thing a new user reads.
    #[test]
    fn the_binary_explains_itself() {
        let command = Cli::command();
        assert!(command.get_about().is_some());
        let long = command
            .get_long_about()
            .expect("no long_about")
            .to_string();
        // The two facts that make the design comprehensible.
        assert!(long.contains("append-only"), "{long}");
        assert!(long.contains("rebuilt from the log"), "{long}");
    }

    #[test]
    fn destructive_commands_say_so() {
        let command = Cli::command();
        let rebuild = command
            .get_subcommands()
            .find(|c| c.get_name() == "rebuild")
            .expect("no rebuild command");
        let text = format!(
            "{} {}",
            rebuild.get_about().unwrap(),
            rebuild.get_long_about().map(|s| s.to_string()).unwrap_or_default()
        );
        assert!(
            text.contains("Discard") || text.contains("discard"),
            "rebuild's help should warn that it discards the wiki: {text}"
        );
    }
}

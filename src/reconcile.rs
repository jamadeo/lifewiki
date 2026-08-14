//! The reconciliation agent, driven by `goose_agent`'s `StateMachine`.
//!
//! The machine tries each step in order on every iteration; the first that
//! applies produces effects and the loop repeats. Our steps are:
//!
//!   1. `MaxTurnsOperation`  — stop before an agent with write access runs away
//!   2. `ToolExecutionOperation` — run the wiki tools the model asked for
//!   3. `FinishOperation`    — the model called `finish`; yield
//!   4. `InferenceRunner`    — otherwise, ask the model what to do next
//!
//! Operations are ours because goose's are not public API. Keeping them small
//! and in one file is deliberate: the interesting behaviour is in the tools and
//! the prompt, not the loop.

use crate::event::Event;
use crate::model::{Backend, Usage};
use crate::progress::Reporter;
use crate::tools::{self, Workspace};
use anyhow::{bail, Result};
use async_trait::async_trait;
use goose_agent::machine::{EffectHandler, MachineSession, SessionLoader, StateMachine, Step};
use goose_agent::operation::{
    applied, ends_turn, not_applicable, ConversationEffect, Emitter, Inference, InferenceInput,
    Operation, OperationResult, StepResult,
};
use goose_providers::conversation::message::{Message, MessageContent, ToolRequest};
use goose_providers::conversation::Conversation;
use rmcp::model::{CallToolResult, ContentBlock, Tool};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_MAX_TURNS: usize = 24;

#[derive(Debug)]
pub struct Outcome {
    pub summary: String,
    pub turns: usize,
    pub usage: Usage,
    pub journal: tools::Journal,
    pub transcript: Vec<String>,
}

/// Everything a step may touch. The `StateMachine` hands `&Session` to each
/// operation, so shared mutable state lives behind mutexes here.
pub struct Session {
    id: String,
    conversation: Conversation,
    workspace: Arc<Mutex<Workspace>>,
    record: Arc<Mutex<Record>>,
    reporter: Arc<Reporter>,
}

#[derive(Default)]
struct Record {
    transcript: Vec<String>,
    usage: Usage,
    turns: usize,
    summary: Option<String>,
}

impl MachineSession for Session {
    fn id(&self) -> &str {
        &self.id
    }
    fn conversation(&self) -> Option<&Conversation> {
        Some(&self.conversation)
    }
}

/// Owns the conversation between machine iterations. `StateMachine::run` calls
/// `load` at the top of each pass and `apply_effects` at the end, so this is
/// where the transcript actually lives.
struct Runtime {
    conversation: Mutex<Conversation>,
    workspace: Arc<Mutex<Workspace>>,
    record: Arc<Mutex<Record>>,
    reporter: Arc<Reporter>,
}

#[async_trait]
impl SessionLoader<Session> for Runtime {
    async fn load(&self, session_id: &str) -> Result<Session> {
        Ok(Session {
            id: session_id.to_string(),
            conversation: self.conversation.lock().unwrap().clone(),
            workspace: self.workspace.clone(),
            record: self.record.clone(),
            reporter: self.reporter.clone(),
        })
    }
}

#[async_trait]
impl EffectHandler<Session, ConversationEffect> for Runtime {
    async fn apply_effects(
        &self,
        _session: &Session,
        effects: &mut [ConversationEffect],
        _emit: &Emitter,
    ) -> Result<()> {
        let mut conversation = self.conversation.lock().unwrap();
        for effect in effects {
            match effect {
                ConversationEffect::AppendMessage(message) => conversation.push(message.clone()),
                ConversationEffect::ReplaceConversation(replacement) => {
                    *conversation = replacement.clone()
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Stop the loop before an agent with write access to the wiki runs away.
struct MaxTurnsOperation {
    limit: usize,
}

#[async_trait]
impl Operation<Session, ConversationEffect> for MaxTurnsOperation {
    fn name(&self) -> &'static str {
        "max_turns"
    }

    async fn run(
        &self,
        session: &Session,
        _conversation: &Conversation,
        _emit: &Emitter,
    ) -> Result<OperationResult<ConversationEffect>> {
        let turns = session.record.lock().unwrap().turns;
        if turns >= self.limit {
            bail!(
                "reconciliation did not finish within {} turns; the wiki may be \
                 partially updated (run `git checkout -- wiki/` to discard)",
                self.limit
            );
        }
        not_applicable()
    }
}

/// Execute pending tool calls against the wiki.
struct ToolExecutionOperation;

impl ToolExecutionOperation {
    /// Tool requests in the last assistant message that have no response yet.
    fn pending(conversation: &Conversation) -> Vec<ToolRequest> {
        let messages = conversation.messages();
        let Some(last) = messages.last() else {
            return Vec::new();
        };
        if last.role != rmcp::model::Role::Assistant {
            return Vec::new();
        }
        last.content
            .iter()
            .filter_map(|content| match content {
                MessageContent::ToolRequest(request) => Some(request.clone()),
                _ => None,
            })
            .collect()
    }
}

#[async_trait]
impl Operation<Session, ConversationEffect> for ToolExecutionOperation {
    fn name(&self) -> &'static str {
        "tool_execution"
    }

    async fn inference_tools(&self, _session: &Session) -> Result<Vec<Tool>> {
        Ok(tools::definitions())
    }

    async fn run(
        &self,
        session: &Session,
        conversation: &Conversation,
        _emit: &Emitter,
    ) -> Result<OperationResult<ConversationEffect>> {
        let pending = Self::pending(conversation);
        // `finish` is handled by its own operation, which must run first.
        if pending.is_empty() || pending.iter().any(|request| is_finish(request)) {
            return not_applicable();
        }

        let mut response = Message::user();
        let turn = session.record.lock().unwrap().turns;

        for request in pending {
            let (name, arguments) = match &request.tool_call {
                Ok(call) => (
                    call.name.to_string(),
                    call.arguments
                        .clone()
                        .map(serde_json::Value::Object)
                        .unwrap_or(serde_json::Value::Null),
                ),
                Err(error) => {
                    let text = format!("Error: invalid tool call: {error}");
                    response = response.with_tool_response(
                        request.id.clone(),
                        Ok(CallToolResult::error(vec![ContentBlock::text(text)])),
                    );
                    continue;
                }
            };

            session.reporter.tool_call(&name, &arguments);

            let (output, is_error) = {
                let mut workspace = session.workspace.lock().unwrap();
                tools::dispatch(&mut workspace, &name, &arguments)
            };

            session.reporter.tool_result(&output, is_error);

            session.record.lock().unwrap().transcript.push(format!(
                "[{turn}] {name}({}) -> {}{}",
                compact(&arguments),
                if is_error { "ERROR " } else { "" },
                first_line(&output)
            ));

            let result = if is_error {
                CallToolResult::error(vec![ContentBlock::text(output)])
            } else {
                CallToolResult::success(vec![ContentBlock::text(output)])
            };
            response = response.with_tool_response(request.id.clone(), Ok(result));
        }

        applied([ConversationEffect::AppendMessage(response)])
    }
}

/// The model called `finish`: record the summary and stop the machine.
struct FinishOperation;

#[async_trait]
impl Operation<Session, ConversationEffect> for FinishOperation {
    fn name(&self) -> &'static str {
        "finish"
    }

    async fn run(
        &self,
        session: &Session,
        conversation: &Conversation,
        _emit: &Emitter,
    ) -> Result<OperationResult<ConversationEffect>> {
        let pending = ToolExecutionOperation::pending(conversation);
        let Some(request) = pending.iter().find(|request| is_finish(request)) else {
            return not_applicable();
        };

        let summary = request
            .tool_call
            .as_ref()
            .ok()
            .and_then(|call| call.arguments.clone())
            .and_then(|arguments| {
                arguments
                    .get("summary")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "(no summary given)".to_string());

        {
            let mut record = session.record.lock().unwrap();
            let turns = record.turns;
            record
                .transcript
                .push(format!("[{turns}] finish: {summary}"));
            record.summary = Some(summary.clone());
            session.reporter.finished(&summary);
        }

        // Answer the tool call so the conversation stays well-formed, then stop.
        let response = Message::user().with_tool_response(
            request.id.clone(),
            Ok(CallToolResult::success(vec![ContentBlock::text("Done.")])),
        );
        Ok(OperationResult::Applied(StepResult {
            effects: vec![ConversationEffect::AppendMessage(response)],
            applied_step: None,
            yield_to_client: true,
        }))
    }
}

/// The model replied with prose and no tool calls, so it has nothing further to
/// do. `StateMachine::step` always reaches the inference step, so without this
/// the loop would ask again forever.
struct EndTurnOperation;

#[async_trait]
impl Operation<Session, ConversationEffect> for EndTurnOperation {
    fn name(&self) -> &'static str {
        "end_turn"
    }

    async fn run(
        &self,
        session: &Session,
        conversation: &Conversation,
        _emit: &Emitter,
    ) -> Result<OperationResult<ConversationEffect>> {
        if !ends_turn(conversation.messages()) {
            return not_applicable();
        }
        let mut record = session.record.lock().unwrap();
        if record.summary.is_none() {
            let text = conversation
                .messages()
                .last()
                .map(|message| message.as_concat_text())
                .unwrap_or_default();
            record.summary = Some(text.trim().to_string());
        }
        Ok(OperationResult::Applied(StepResult {
            effects: Vec::new(),
            applied_step: None,
            yield_to_client: true,
        }))
    }
}

/// Ask the model what to do next.
struct InferenceRunner {
    backend: Arc<dyn Backend>,
    system: String,
}

#[async_trait]
impl Operation<Session, ConversationEffect> for InferenceRunner {
    fn name(&self) -> &'static str {
        "inference"
    }
}

#[async_trait]
impl Inference<Session, ConversationEffect> for InferenceRunner {
    fn applies(&self, conversation: &Conversation) -> bool {
        // Only infer when the model isn't already mid-turn: if the last message
        // ends the turn with no tool calls, we're done rather than looping.
        !ends_turn(conversation.messages())
    }

    async fn infer(
        &self,
        session: &Session,
        conversation: &Conversation,
        input: InferenceInput,
        _emit: &Emitter,
    ) -> Result<OperationResult<ConversationEffect>> {
        let next_turn = session.record.lock().unwrap().turns + 1;
        session.reporter.thinking(next_turn);

        let (reply, usage) = self
            .backend
            .complete(&self.system, conversation.messages(), &input.tools)
            .await?;
        session.reporter.model_text(&reply.as_concat_text());

        {
            let mut record = session.record.lock().unwrap();
            record.turns += 1;
            if let Some(input_tokens) = usage.input_tokens {
                *record.usage.input_tokens.get_or_insert(0) += input_tokens;
            }
            if let Some(output_tokens) = usage.output_tokens {
                *record.usage.output_tokens.get_or_insert(0) += output_tokens;
            }
            let text = reply.as_concat_text();
            if !text.trim().is_empty() {
                let turns = record.turns;
                record.transcript.push(format!("[{turns}] {}", text.trim()));
            }
        }

        applied([ConversationEffect::AppendMessage(reply)])
    }
}

fn is_finish(request: &ToolRequest) -> bool {
    request
        .tool_call
        .as_ref()
        .map(|call| call.name == "finish")
        .unwrap_or(false)
}

pub struct Reconciler {
    backend: Arc<dyn Backend>,
    pub max_turns: usize,
    pub reporter: Arc<Reporter>,
}

impl Reconciler {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self {
            backend,
            max_turns: DEFAULT_MAX_TURNS,
            reporter: Arc::new(Reporter::quiet()),
        }
    }

    pub fn run(&self, workspace: Workspace, events: &[Event]) -> Result<(Outcome, Workspace)> {
        if events.is_empty() {
            bail!("no events to reconcile");
        }

        let kickoff = Message::user().with_text(kickoff(events, &self.backend.actor(), &now()));
        let workspace = Arc::new(Mutex::new(workspace));
        let record = Arc::new(Mutex::new(Record::default()));

        self.reporter
            .build_started(events.len(), &self.backend.actor());

        let runtime = Runtime {
            conversation: Mutex::new(Conversation::new_unvalidated([kickoff])),
            workspace: workspace.clone(),
            record: record.clone(),
            reporter: self.reporter.clone(),
        };

        let inference = Arc::new(InferenceRunner {
            backend: self.backend.clone(),
            system: SYSTEM.to_string(),
        });

        let steps: Vec<Step<'_, Session, ConversationEffect>> = vec![
            Step::Operation(Arc::new(MaxTurnsOperation {
                limit: self.max_turns,
            })),
            Step::Operation(Arc::new(FinishOperation)),
            Step::Operation(Arc::new(EndTurnOperation)),
            Step::Operation(Arc::new(ToolExecutionOperation)),
            Step::Inference(inference),
        ];

        let cancel = CancellationToken::new();
        let machine = StateMachine::new(steps, cancel.clone());

        // The machine is async; the CLI is not. A current-thread runtime keeps
        // `lw build` a plain blocking command.
        let (tx, mut rx) = mpsc::channel(64);
        let emitter = Emitter::new(tx, cancel);
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async {
                let outcome = machine.run(&runtime, "reconcile", &emitter).await;
                rx.close();
                while rx.recv().await.is_some() {}
                outcome
            });
        result?;

        // `runtime` still holds clones of these Arcs, so copy the data out
        // rather than trying to take ownership.
        let record = {
            let guard = record.lock().unwrap();
            Record {
                transcript: guard.transcript.clone(),
                usage: guard.usage,
                turns: guard.turns,
                summary: guard.summary.clone(),
            }
        };
        let workspace = {
            let guard = workspace.lock().unwrap();
            guard.clone()
        };

        let summary = record.summary.clone().unwrap_or_else(|| {
            // The model stopped without calling finish; its last words are the
            // best summary available.
            record
                .transcript
                .last()
                .cloned()
                .unwrap_or_else(|| "(no summary)".to_string())
        });

        let outcome = Outcome {
            summary,
            turns: record.turns,
            usage: record.usage,
            journal: workspace.journal.clone(),
            transcript: record.transcript,
        };
        Ok((outcome, workspace))
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if text.lines().count() > 1 {
        format!("{line} …")
    } else {
        line.to_string()
    }
}

fn compact(arguments: &serde_json::Value) -> String {
    let rendered = arguments.to_string();
    if rendered.len() > 120 {
        format!("{}…", &rendered[..120])
    } else {
        rendered
    }
}

fn now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub const SYSTEM: &str = r#"You maintain a personal wiki that is derived from an append-only log of things
its owner wrote about their own life.

You will be given one or more new log events. Your job is to make the wiki
reflect them — which usually means editing existing pages, not just adding new
ones.

THE CENTRAL TASK IS RECONCILIATION.
A new event often contradicts, updates, or completes something the wiki already
says. The log records that the owner once preferred oranges and now prefers
apples; the wiki must end up saying they prefer apples. Nobody will tell you
which page is affected, or that a contradiction exists at all. Finding it is
your job:

1. Search the wiki for the people, places, topics and objects the event mentions.
2. Read every page that might already say something about them.
3. Only then decide what to write.

A stale fact left behind because you did not look for it is the worst possible
outcome. It is far better to search several times and find nothing.

HOW TO RESOLVE A CONTRADICTION depends on the kind of fact, indicated by the
page's `type`:
  - Person, Place, Project, Reference: keep the CURRENT state. Correct the old
    value in place; do not accumulate a history of small corrections.
  - Opinion, Preference, Habit: keep a DATED HISTORY. How the owner's views
    changed over time is itself the content. For example:
    "Prefers apples (since Aug 2026); previously preferred oranges."
  - Journal: never rewrite. These are records of a particular day.

PAGE FORMAT. Every page is markdown with YAML frontmatter:

---
type: Person
title: Ada
description: One line, used in listings.
tags: [family]
generated: { by: <the actor string you are given>, at: <the timestamp you are given> }
sources:
  - id: e01K2C57T
    resource: /log/2026-08.jsonl#01K2C57TDA6P1YQ8W3ZR4H2J9C
    title: "the event text, quoted"
---

# Facts

Ada's birthday is 14 March.[^e01K2C57T]

[^e01K2C57T]: "Ada's birthday is the 14th of March, not the 4th."

RULES FOR WRITING:
  - `type` is required and must not be empty.
  - Cite the event behind a claim with a footnote whose label matches a
    `sources` entry id. Use `e` followed by the first 8 characters of the event
    id. A footnote with no matching source will be rejected.
  - When you rewrite a page, keep the sources for claims that remain, and drop
    sources whose claims are gone.
  - write_page replaces the whole file. Include everything the page should
    still say, not just your changes.
  - Prefer few, substantial pages over many thin ones. Add to an existing page
    when the subject already has one.
  - Use paths like /people/ada.md, /projects/lifewiki.md, /opinions/food.md.

IF YOU CANNOT PLACE AN EVENT — it is too vague to attach to anything, such as
"I changed my mind about that" with no referent — do not guess. Append it to
/unresolved.md with its event id and a note, so the owner can clarify later.
Guessing wrong silently is worse than admitting you could not tell.

When the wiki fully reflects the events, call `finish` with a short summary of
what you changed and why."#;

fn kickoff(events: &[Event], actor: &str, timestamp: &str) -> String {
    let mut text = format!(
        "Fold these new log events into the wiki. Search before you write.\n\n\
         When you write or rewrite a page, set its frontmatter `generated` to:\n\
         generated: {{ by: {actor}, at: {timestamp} }}\n\n\
         New events:\n\n"
    );
    for event in events {
        text.push_str(&format!(
            "- id: {}\n  recorded: {}\n  footnote label: e{}\n  text: {:?}\n",
            event.id,
            event.ts,
            &event.id[..8],
            event.text()
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tests::ScriptedBackend;

    fn event(text: &str) -> Event {
        Event {
            id: "01K2C57TDA6P1YQ8W3ZR4H2J9C".into(),
            ts: "2026-08-11T13:02:44-04:00".into(),
            text: Some(text.into()),
            redacted: None,
        }
    }

    fn scratch() -> crate::tools::tests::tempdir::Dir {
        crate::tools::tests::tempdir::Dir::new()
    }

    #[test]
    fn writes_a_page_and_finishes() {
        let dir = scratch();
        let workspace = Workspace::new(dir.path()).unwrap();
        let backend = ScriptedBackend::new(vec![
            ScriptedBackend::tool_call("search_wiki", serde_json::json!({"query": "apples"})),
            ScriptedBackend::tool_call(
                "write_page",
                serde_json::json!({
                    "path": "/opinions/food.md",
                    "content": "---\ntype: Preference\n---\n\nPrefers apples.\n"
                }),
            ),
            ScriptedBackend::tool_call(
                "finish",
                serde_json::json!({"summary": "Recorded fruit preference."}),
            ),
        ]);

        let (outcome, _) = Reconciler::new(backend)
            .run(workspace, &[event("I prefer apples")])
            .unwrap();

        assert_eq!(outcome.summary, "Recorded fruit preference.");
        assert_eq!(outcome.journal.writes, vec!["/opinions/food.md"]);
        assert!(dir.path().join("opinions/food.md").exists());
        assert_eq!(outcome.turns, 3);
    }

    #[test]
    fn invalid_page_is_reported_to_the_model_not_written() {
        let dir = scratch();
        let workspace = Workspace::new(dir.path()).unwrap();
        let backend = ScriptedBackend::new(vec![
            ScriptedBackend::tool_call(
                "write_page",
                serde_json::json!({"path": "/a.md", "content": "no frontmatter"}),
            ),
            ScriptedBackend::tool_call("finish", serde_json::json!({"summary": "gave up"})),
        ]);

        let (outcome, _) = Reconciler::new(backend)
            .run(workspace, &[event("something")])
            .unwrap();

        assert!(
            !dir.path().join("a.md").exists(),
            "invalid page was written"
        );
        assert!(outcome.journal.writes.is_empty());
        assert!(
            outcome.transcript.iter().any(|line| line.contains("ERROR")),
            "the failure should appear in the transcript: {:?}",
            outcome.transcript
        );
    }

    #[test]
    fn turn_limit_is_enforced() {
        let dir = scratch();
        let workspace = Workspace::new(dir.path()).unwrap();
        let backend = ScriptedBackend::new(
            (0..10)
                .map(|_| ScriptedBackend::tool_call("list_pages", serde_json::json!({})))
                .collect(),
        );

        let mut reconciler = Reconciler::new(backend);
        reconciler.max_turns = 3;
        let error = reconciler
            .run(workspace, &[event("x")])
            .unwrap_err()
            .to_string();
        assert!(error.contains("within 3 turns"), "{error}");
    }

    #[test]
    fn plain_text_reply_ends_the_run() {
        let dir = scratch();
        let workspace = Workspace::new(dir.path()).unwrap();
        let backend = ScriptedBackend::new(vec![ScriptedBackend::text("Nothing to change.")]);
        let (outcome, _) = Reconciler::new(backend)
            .run(workspace, &[event("x")])
            .unwrap();
        assert_eq!(outcome.turns, 1);
        assert!(
            outcome.summary.contains("Nothing to change."),
            "{:?}",
            outcome.summary
        );
    }

    #[test]
    fn tool_results_reach_the_model() {
        let dir = scratch();
        std::fs::write(
            dir.path().join("food.md"),
            "---\ntype: Preference\n---\n\nPrefers oranges.\n",
        )
        .unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let backend = ScriptedBackend::new(vec![
            ScriptedBackend::tool_call("search_wiki", serde_json::json!({"query": "oranges"})),
            ScriptedBackend::tool_call("finish", serde_json::json!({"summary": "done"})),
        ]);
        let backend_handle = backend.clone();

        let (outcome, _) = Reconciler::new(backend)
            .run(workspace, &[event("I prefer apples now")])
            .unwrap();

        assert_eq!(outcome.turns, 2);
        // The second inference must have seen the search result.
        let seen = backend_handle.last_messages();
        let rendered = seen
            .iter()
            .map(|message| format!("{:?}", message.content))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("food.md"),
            "the tool response was not fed back: {rendered}"
        );
    }
}

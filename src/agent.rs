//! The agent loop, driven by `goose_agent`'s `StateMachine`.
//!
//! One loop serves both tasks. A build reconciles new log entries into the wiki;
//! `lw ask` answers a question from it. They differ in only three ways, which is
//! exactly what [`Role`] carries:
//!
//!   * which tools are offered
//!   * the system prompt
//!   * the name of the tool that ends the run, and which argument holds its result
//!
//! Operations are ours because goose's are not public API yet. The machine tries
//! each step in order per iteration; the first that applies produces effects and
//! the loop repeats.
//!
//! One sharp edge: `StateMachine::step` always reaches the inference step and
//! does not consult `Inference::applies`, so a prose reply with no tool calls
//! would loop forever without [`EndTurnOperation`] ahead of it.

use crate::model::{Backend, Usage};
use crate::progress::Reporter;
use crate::tools::{self, Journal, Workspace};
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

/// What the agent is for. The only difference between a build and a question.
#[derive(Clone, Copy)]
pub struct Role {
    /// Tools offered to the model.
    pub tools: fn() -> Vec<Tool>,
    /// Instructions.
    pub system: &'static str,
    /// The tool that ends the run.
    pub terminal_tool: &'static str,
    /// Which of that tool's arguments carries the result.
    pub terminal_argument: &'static str,
    /// Give up after this many turns.
    pub max_turns: usize,
    /// Start warning the model when this many turns remain.
    pub warn_at: usize,
    /// How to phrase the warning. `{remaining}` is substituted.
    pub warning: &'static str,
    /// What running out means for this role.
    pub exhausted: Exhausted,
}

/// What to do when a run uses its whole budget without calling its terminal
/// tool.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Exhausted {
    /// Fail. A half-finished build has written pages without knowing whether
    /// they are consistent, so the cursor must not advance.
    Fail,
    /// Report the given text as the result. For a question, running out of
    /// searches without finding anything *is* the answer, and telling the user
    /// that is more useful than an error.
    Report(&'static str),
}

pub const BUILD: Role = Role {
    tools: tools::definitions,
    system: crate::prompts::BUILD,
    terminal_tool: "finish",
    terminal_argument: "summary",
    // A build typically takes 5-6 turns. The ceiling is generous because a
    // large batch of events legitimately needs more, but the warning starts
    // well before it.
    max_turns: 16,
    warn_at: 4,
    exhausted: Exhausted::Fail,
    warning: "You have {remaining} turns left before this run is stopped. Finish \
              the most important edits now and call `finish`. A partial, \
              accurate wiki is better than an interrupted one — anything you \
              leave undone can be folded by a later build, but only if you \
              say what you did not get to.",
};

pub const ASK: Role = Role {
    tools: tools::ask_definitions,
    system: crate::prompts::ASK,
    terminal_tool: "answer",
    terminal_argument: "text",
    // An answerable question takes about 3 turns. Beyond that the model is
    // usually casting around for something the wiki does not contain, and a
    // small model will exhaust its context around turn 8 — so both the limit
    // and the warning sit below that, or neither would ever be reached.
    max_turns: 8,
    // A normal question resolves in about 3 turns, so warning at 5 remaining
    // fired on the *third* turn of a healthy run and derailed it: the model
    // replied to the warning instead of the question. Only warn once a run is
    // clearly overrunning.
    warn_at: 2,
    exhausted: Exhausted::Report(
        "I could not find an answer in the wiki. I searched but did not find \
         anything covering this, so it is probably not recorded — or it is \
         phrased differently than I looked for.",
    ),
    warning: "You have {remaining} searches left before this run is stopped. If \
              you have not found the answer by now, the wiki probably does not \
              contain it — say so plainly with `answer`. Telling the user the \
              wiki does not cover something is a correct and useful answer; \
              guessing from related facts is not.",
};

#[derive(Debug)]
pub struct Outcome {
    /// The terminal tool's result: a summary of changes, or an answer.
    pub result: String,
    pub turns: usize,
    pub usage: Usage,
    pub journal: Journal,
}

#[derive(Default)]
struct Record {
    usage: Usage,
    turns: usize,
    result: Option<String>,
    /// Remaining-turn count at the last warning, so each is issued once.
    warned_at: Option<usize>,
}

/// Everything a step may touch. `StateMachine` hands `&Session` to each
/// operation, so shared mutable state lives behind mutexes.
pub struct Session {
    id: String,
    conversation: Conversation,
    workspace: Arc<Mutex<Workspace>>,
    record: Arc<Mutex<Record>>,
    reporter: Arc<Reporter>,
    role: Role,
}

impl MachineSession for Session {
    fn id(&self) -> &str {
        &self.id
    }
    fn conversation(&self) -> Option<&Conversation> {
        Some(&self.conversation)
    }
}

/// Owns the conversation between machine iterations: `load` is called at the top
/// of each pass and `apply_effects` at the end, so the transcript lives here
/// rather than being threaded through the loop.
struct Runtime {
    conversation: Mutex<Conversation>,
    workspace: Arc<Mutex<Workspace>>,
    record: Arc<Mutex<Record>>,
    reporter: Arc<Reporter>,
    role: Role,
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
            role: self.role,
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

fn calls(request: &ToolRequest, name: &str) -> bool {
    request
        .tool_call
        .as_ref()
        .map(|call| call.name == name)
        .unwrap_or(false)
}

/// Stop before an agent runs away, and warn it as the limit approaches.
///
/// Hitting the limit is a failure: the run is discarded and the user waited for
/// nothing. Telling the model how many turns remain lets it land somewhere
/// useful instead — a partial summary, or an honest "the wiki does not say".
/// Without this a small model will happily search until it exhausts its context.
struct MaxTurnsOperation;

#[async_trait]
impl Operation<Session, ConversationEffect> for MaxTurnsOperation {
    fn name(&self) -> &'static str {
        "max_turns"
    }

    async fn run(
        &self,
        session: &Session,
        conversation: &Conversation,
        _emit: &Emitter,
    ) -> Result<OperationResult<ConversationEffect>> {
        let role = session.role;
        let mut record = session.record.lock().unwrap();
        if record.turns >= role.max_turns {
            return match role.exhausted {
                Exhausted::Fail => bail!(
                    "gave up after {} turns without finishing",
                    role.max_turns
                ),
                Exhausted::Report(text) => {
                    // Only if nothing was answered: a model that answered and
                    // then kept talking has still answered.
                    if record.result.is_none() {
                        record.result = Some(text.to_string());
                    }
                    drop(record);
                    Ok(OperationResult::Applied(StepResult {
                        effects: Vec::new(),
                        applied_step: None,
                        yield_to_client: true,
                    }))
                }
            };
        }

        let remaining = role.max_turns - record.turns;
        if remaining > role.warn_at || record.warned_at == Some(remaining) {
            return not_applicable();
        }
        drop(record);

        // Never nag a model that has stopped asking for tools. Warning after a
        // prose reply restarts a finished conversation, and the model — having
        // already answered — spends the rest of its budget saying so.
        if ends_turn(conversation.messages()) {
            return not_applicable();
        }
        let mut record = session.record.lock().unwrap();
        // Once per turn, not once per run: a model that ignores the first
        // warning should see the number fall.
        record.warned_at = Some(remaining);
        drop(record);

        let text = role.warning.replace("{remaining}", &remaining.to_string());
        session.reporter.warn(&text);
        applied([ConversationEffect::AppendMessage(
            Message::user().with_text(text),
        )])
    }
}

/// The model called the terminal tool: record its result and stop.
struct TerminalOperation;

#[async_trait]
impl Operation<Session, ConversationEffect> for TerminalOperation {
    fn name(&self) -> &'static str {
        "terminal"
    }

    async fn run(
        &self,
        session: &Session,
        conversation: &Conversation,
        _emit: &Emitter,
    ) -> Result<OperationResult<ConversationEffect>> {
        let role = session.role;
        let requests = pending(conversation);
        let Some(request) = requests
            .iter()
            .find(|request| calls(request, role.terminal_tool))
        else {
            return not_applicable();
        };

        let result = request
            .tool_call
            .as_ref()
            .ok()
            .and_then(|call| call.arguments.clone())
            .and_then(|arguments| {
                arguments
                    .get(role.terminal_argument)
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default();

        session.reporter.finished(&result);
        session.record.lock().unwrap().result = Some(result);

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
/// do. Take its words as the result rather than discarding them.
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
        if record.result.is_none() {
            let text = conversation
                .messages()
                .last()
                .map(|message| message.as_concat_text())
                .unwrap_or_default();
            record.result = Some(text.trim().to_string());
        }
        Ok(OperationResult::Applied(StepResult {
            effects: Vec::new(),
            applied_step: None,
            yield_to_client: true,
        }))
    }
}

/// Run the tools the model asked for.
struct ToolExecutionOperation;

#[async_trait]
impl Operation<Session, ConversationEffect> for ToolExecutionOperation {
    fn name(&self) -> &'static str {
        "tool_execution"
    }

    async fn inference_tools(&self, session: &Session) -> Result<Vec<Tool>> {
        Ok((session.role.tools)())
    }

    async fn run(
        &self,
        session: &Session,
        conversation: &Conversation,
        _emit: &Emitter,
    ) -> Result<OperationResult<ConversationEffect>> {
        let requests = pending(conversation);
        // The terminal tool is handled by its own operation, which runs first.
        if requests.is_empty()
            || requests
                .iter()
                .any(|request| calls(request, session.role.terminal_tool))
        {
            return not_applicable();
        }

        let mut response = Message::user();
        for request in requests {
            let (name, arguments) = match &request.tool_call {
                Ok(call) => (
                    call.name.to_string(),
                    call.arguments
                        .clone()
                        .map(serde_json::Value::Object)
                        .unwrap_or(serde_json::Value::Null),
                ),
                Err(error) => {
                    response = response.with_tool_response(
                        request.id.clone(),
                        Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                            "Error: invalid tool call: {error}"
                        ))])),
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

/// Ask the model what to do next.
struct InferenceRunner {
    backend: Arc<dyn Backend>,
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
            .complete(session.role.system, conversation.messages(), &input.tools)
            .await?;
        session.reporter.model_text(&reply.as_concat_text());

        let mut record = session.record.lock().unwrap();
        record.turns += 1;
        if let Some(input_tokens) = usage.input_tokens {
            *record.usage.input_tokens.get_or_insert(0) += input_tokens;
        }
        if let Some(output_tokens) = usage.output_tokens {
            *record.usage.output_tokens.get_or_insert(0) += output_tokens;
        }
        drop(record);

        applied([ConversationEffect::AppendMessage(reply)])
    }
}

pub struct Agent {
    backend: Arc<dyn Backend>,
    role: Role,
    pub reporter: Arc<Reporter>,
}

impl Agent {
    pub fn new(backend: Arc<dyn Backend>, role: Role) -> Self {
        Self {
            backend,
            role,
            reporter: Arc::new(Reporter::silent()),
        }
    }

    pub fn actor(&self) -> String {
        self.backend.actor()
    }

    /// Run until the model finishes, gives up, or exceeds its turn limit.
    ///
    /// Takes the workspace by value and hands it back, so a caller can inspect
    /// what happened without sharing mutable state with the loop.
    pub fn run(&self, workspace: Workspace, opening: &str) -> Result<(Outcome, Workspace)> {
        let workspace = Arc::new(Mutex::new(workspace));
        let record = Arc::new(Mutex::new(Record::default()));

        let runtime = Runtime {
            conversation: Mutex::new(Conversation::new_unvalidated([
                Message::user().with_text(opening)
            ])),
            workspace: workspace.clone(),
            record: record.clone(),
            reporter: self.reporter.clone(),
            role: self.role,
        };

        let steps: Vec<Step<'_, Session, ConversationEffect>> = vec![
            Step::Operation(Arc::new(MaxTurnsOperation)),
            Step::Operation(Arc::new(TerminalOperation)),
            Step::Operation(Arc::new(EndTurnOperation)),
            Step::Operation(Arc::new(ToolExecutionOperation)),
            Step::Inference(Arc::new(InferenceRunner {
                backend: self.backend.clone(),
            })),
        ];

        let cancel = CancellationToken::new();
        let machine = StateMachine::new(steps, cancel.clone());
        let (tx, mut rx) = mpsc::channel(64);
        let emitter = Emitter::new(tx, cancel);

        // The machine is async; the CLI is not.
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async {
                let outcome = machine.run(&runtime, "lifewiki", &emitter).await;
                rx.close();
                while rx.recv().await.is_some() {}
                outcome
            })?;

        // `runtime` holds clones of these, so copy the data out.
        let (result, turns, usage) = {
            let record = record.lock().unwrap();
            (record.result.clone(), record.turns, record.usage)
        };
        let workspace = workspace.lock().unwrap().clone();

        Ok((
            Outcome {
                result: result.unwrap_or_else(|| "(no result)".to_string()),
                turns,
                usage,
                journal: workspace.journal.clone(),
            },
            workspace,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tests::ScriptedBackend;
    use crate::tools::tests::tempdir::Dir;

    fn wiki() -> Dir {
        let dir = Dir::new();
        std::fs::create_dir_all(dir.path().join("people")).unwrap();
        std::fs::write(
            dir.path().join("people/ada.md"),
            "---\ntype: Person\ntitle: Ada\n---\n\nAda lives in Porto.\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn build_writes_a_page_and_finishes() {
        let dir = Dir::new();
        let backend = ScriptedBackend::new(vec![
            ScriptedBackend::tool_call("search_wiki", serde_json::json!({"query": "apples"})),
            ScriptedBackend::tool_call(
                "write_page",
                serde_json::json!({
                    "path": "/opinions/food.md",
                    "content": "---\ntype: Preference\n---\n\nPrefers apples.\n"
                }),
            ),
            ScriptedBackend::tool_call("finish", serde_json::json!({"summary": "Recorded it."})),
        ]);

        let mut workspace = Workspace::new(dir.path()).unwrap();
        workspace.writable = true;
        let (outcome, _) = Agent::new(backend, BUILD)
            .run(workspace, "fold this")
            .unwrap();

        assert_eq!(outcome.result, "Recorded it.");
        assert_eq!(outcome.journal.writes, vec!["/opinions/food.md"]);
        assert_eq!(outcome.turns, 3);
    }

    #[test]
    fn ask_reads_and_answers() {
        let dir = wiki();
        let backend = ScriptedBackend::new(vec![
            ScriptedBackend::tool_call("read_page", serde_json::json!({"path": "/people/ada.md"})),
            ScriptedBackend::tool_call("answer", serde_json::json!({"text": "Porto."})),
        ]);

        let mut workspace = Workspace::new(dir.path()).unwrap();
        workspace.writable = false;
        let (outcome, _) = Agent::new(backend, ASK).run(workspace, "where?").unwrap();

        assert_eq!(outcome.result, "Porto.");
        assert_eq!(outcome.journal.reads, vec!["/people/ada.md"]);
    }

    /// A question must never change the wiki, whatever the model does.
    #[test]
    fn ask_cannot_write() {
        let dir = wiki();
        let before = std::fs::read_to_string(dir.path().join("people/ada.md")).unwrap();
        let backend = ScriptedBackend::new(vec![
            ScriptedBackend::tool_call(
                "write_page",
                serde_json::json!({"path": "/people/ada.md", "content": "---\ntype: X\n---\n"}),
            ),
            ScriptedBackend::tool_call("answer", serde_json::json!({"text": "done"})),
        ]);

        let mut workspace = Workspace::new(dir.path()).unwrap();
        workspace.writable = false;
        Agent::new(backend, ASK).run(workspace, "anything").unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("people/ada.md")).unwrap(),
            before
        );
    }

    #[test]
    fn ask_is_not_offered_write_tools() {
        let names: Vec<String> = (ASK.tools)()
            .iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert!(names.contains(&"read_event".to_string()));
        assert!(!names.contains(&"write_page".to_string()));
        assert!(!names.contains(&"delete_page".to_string()));
    }

    #[test]
    fn an_invalid_page_is_reported_to_the_model_not_written() {
        let dir = Dir::new();
        let backend = ScriptedBackend::new(vec![
            ScriptedBackend::tool_call(
                "write_page",
                serde_json::json!({"path": "/a.md", "content": "no frontmatter"}),
            ),
            ScriptedBackend::tool_call("finish", serde_json::json!({"summary": "gave up"})),
        ]);

        let mut workspace = Workspace::new(dir.path()).unwrap();
        workspace.writable = true;
        let (outcome, _) = Agent::new(backend, BUILD).run(workspace, "fold").unwrap();

        assert!(!dir.path().join("a.md").exists());
        assert!(outcome.journal.writes.is_empty());
    }

    /// `StateMachine::step` always reaches inference, so prose with no tool call
    /// would loop forever without `EndTurnOperation`.
    #[test]
    fn prose_with_no_tool_call_ends_the_run() {
        let dir = Dir::new();
        let backend = ScriptedBackend::new(vec![ScriptedBackend::text("Nothing to change.")]);
        let workspace = Workspace::new(dir.path()).unwrap();
        let (outcome, _) = Agent::new(backend, BUILD).run(workspace, "fold").unwrap();
        assert_eq!(outcome.turns, 1);
        assert!(outcome.result.contains("Nothing to change."));
    }

    #[test]
    fn tool_results_reach_the_next_inference() {
        let dir = Dir::new();
        std::fs::write(
            dir.path().join("food.md"),
            "---\ntype: Preference\n---\n\nPrefers oranges.\n",
        )
        .unwrap();
        let backend = ScriptedBackend::new(vec![
            ScriptedBackend::tool_call("search_wiki", serde_json::json!({"query": "oranges"})),
            ScriptedBackend::tool_call("finish", serde_json::json!({"summary": "done"})),
        ]);
        let handle = backend.clone();

        let workspace = Workspace::new(dir.path()).unwrap();
        Agent::new(backend, BUILD).run(workspace, "fold").unwrap();

        let rendered = handle
            .last_messages()
            .iter()
            .map(|message| format!("{:?}", message.content))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("food.md"),
            "the tool response was not fed back: {rendered}"
        );
    }

    /// The model must be told the limit is coming, in the conversation itself
    /// — a model that cannot see the budget cannot spend it well.
    #[test]
    fn the_model_is_warned_as_the_limit_approaches() {
        let dir = Dir::new();
        let backend = ScriptedBackend::new(
            (0..30)
                .map(|_| ScriptedBackend::tool_call("list_pages", serde_json::json!({})))
                .collect(),
        );
        let handle = backend.clone();

        let workspace = Workspace::new(dir.path()).unwrap();
        let _ = Agent::new(backend, ASK).run(workspace, "loop");

        let rendered = handle
            .last_messages()
            .iter()
            .map(|message| message.as_concat_text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("searches left"),
            "no warning reached the model: {rendered}"
        );
        assert!(
            rendered.contains("does not cover something is a correct"),
            "the warning should tell it that admitting ignorance is fine"
        );
    }

    /// Counting down matters: a model that ignored "4 left" should see "3 left".
    #[test]
    fn the_warning_counts_down() {
        let dir = Dir::new();
        let backend = ScriptedBackend::new(
            (0..30)
                .map(|_| ScriptedBackend::tool_call("list_pages", serde_json::json!({})))
                .collect(),
        );
        let handle = backend.clone();

        let workspace = Workspace::new(dir.path()).unwrap();
        let _ = Agent::new(backend, ASK).run(workspace, "loop");

        let rendered = handle
            .last_messages()
            .iter()
            .map(|message| message.as_concat_text())
            .collect::<Vec<_>>()
            .join("\n");
        for remaining in (1..=ASK.warn_at).rev() {
            let expected = format!("{remaining} searches left");
            assert!(rendered.contains(&expected), "missing {expected:?}");
        }
    }

    /// A run that finishes early should never see a warning.
    #[test]
    fn a_short_run_is_not_warned() {
        let dir = Dir::new();
        let backend = ScriptedBackend::new(vec![ScriptedBackend::tool_call(
            "answer",
            serde_json::json!({"text": "done"}),
        )]);
        let handle = backend.clone();

        let workspace = Workspace::new(dir.path()).unwrap();
        Agent::new(backend, ASK).run(workspace, "quick").unwrap();

        let rendered = handle
            .last_messages()
            .iter()
            .map(|message| message.as_concat_text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!rendered.contains("searches left"), "{rendered}");
    }



    fn looping_backend() -> std::sync::Arc<ScriptedBackend> {
        ScriptedBackend::new(
            (0..40)
                .map(|_| ScriptedBackend::tool_call("list_pages", serde_json::json!({})))
                .collect(),
        )
    }

    /// A build that runs out has written pages without knowing whether they are
    /// consistent, so it must fail and leave the cursor alone.
    #[test]
    fn an_exhausted_build_fails() {
        let dir = Dir::new();
        let mut workspace = Workspace::new(dir.path()).unwrap();
        workspace.writable = true;
        let error = Agent::new(looping_backend(), BUILD)
            .run(workspace, "loop")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!("{} turns", BUILD.max_turns)),
            "{error}"
        );
    }

    /// A question that runs out has an answer: the wiki does not say. Reporting
    /// that is more useful than an error the user cannot act on.
    #[test]
    fn an_exhausted_question_reports_that_it_found_nothing() {
        let dir = Dir::new();
        let workspace = Workspace::new(dir.path()).unwrap();
        let (outcome, _) = Agent::new(looping_backend(), ASK)
            .run(workspace, "loop")
            .expect("running out of searches is not an error for a question");
        assert!(
            outcome.result.contains("could not find an answer"),
            "{}",
            outcome.result
        );
        assert_eq!(outcome.turns, ASK.max_turns, "the budget was spent");
    }

    /// A model that has answered must not be nagged: warning after a prose
    /// reply restarts a finished conversation, and the model spends the rest of
    /// its budget explaining that it already answered.
    #[test]
    fn a_model_that_has_finished_is_not_warned() {
        let dir = Dir::new();
        // Enough tool calls to reach the warning zone, then a prose reply.
        let mut script: Vec<_> = (0..ASK.max_turns - ASK.warn_at)
            .map(|_| ScriptedBackend::tool_call("list_pages", serde_json::json!({})))
            .collect();
        script.push(ScriptedBackend::text("Biscuit is a beagle."));
        let backend = ScriptedBackend::new(script);

        let workspace = Workspace::new(dir.path()).unwrap();
        let (outcome, _) = Agent::new(backend, ASK).run(workspace, "who?").unwrap();

        assert!(
            outcome.result.contains("beagle"),
            "the answer was discarded: {}",
            outcome.result
        );
        assert!(
            outcome.turns < ASK.max_turns,
            "the run should stop when the model answers, not burn its budget"
        );
    }

    /// Exhausting the budget must not overwrite an answer already given.
    #[test]
    fn an_answer_survives_running_out_of_turns() {
        let dir = Dir::new();
        let mut script = vec![ScriptedBackend::tool_call(
            "answer",
            serde_json::json!({"text": "Porto."}),
        )];
        script.extend(
            (0..40).map(|_| ScriptedBackend::tool_call("list_pages", serde_json::json!({}))),
        );
        let backend = ScriptedBackend::new(script);

        let workspace = Workspace::new(dir.path()).unwrap();
        let (outcome, _) = Agent::new(backend, ASK).run(workspace, "where?").unwrap();
        assert_eq!(outcome.result, "Porto.");
    }

    /// The warning has to fit inside the budget, or it can never fire.
    #[test]
    fn every_role_warns_before_it_gives_up() {
        for role in [BUILD, ASK] {
            assert!(
                role.warn_at < role.max_turns,
                "a warning at {} turns remaining never fires with a {} turn limit",
                role.warn_at,
                role.max_turns
            );
            assert!(role.warning.contains("{remaining}"));
        }
    }
}

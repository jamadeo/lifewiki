//! `lw ask` — answer a question from the wiki (phase 3).
//!
//! The same `goose_agent` state machine as reconciliation, with a different
//! tool set and a hard guarantee: nothing here can write. The workspace is
//! constructed read-only, and the write tools are simply not offered, so a
//! question can never modify the wiki however the model behaves.
//!
//! The interesting difference from a build is what counts as a good answer.
//! A build must be thorough — a missed contradiction leaves a stale fact. An
//! answer must be *honest*: saying "the wiki doesn't say" is correct and useful,
//! whereas a plausible guess assembled from adjacent facts is worse than
//! nothing, because the whole point is recalling your own life accurately.

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
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_MAX_TURNS: usize = 16;

#[derive(Debug)]
pub struct Answer {
    pub text: String,
    pub turns: usize,
    pub usage: Usage,
    pub pages_read: Vec<String>,
}

struct Record {
    usage: Usage,
    turns: usize,
    answer: Option<String>,
    pages_read: Vec<String>,
}

pub struct Session {
    id: String,
    conversation: Conversation,
    workspace: Arc<Mutex<Workspace>>,
    record: Arc<Mutex<Record>>,
    reporter: Arc<Reporter>,
}

impl MachineSession for Session {
    fn id(&self) -> &str {
        &self.id
    }
    fn conversation(&self) -> Option<&Conversation> {
        Some(&self.conversation)
    }
}

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
        if session.record.lock().unwrap().turns >= self.limit {
            bail!(
                "gave up after {} turns without reaching an answer",
                self.limit
            );
        }
        not_applicable()
    }
}

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

fn is_answer(request: &ToolRequest) -> bool {
    request
        .tool_call
        .as_ref()
        .map(|call| call.name == "answer")
        .unwrap_or(false)
}

struct AnswerOperation;

#[async_trait]
impl Operation<Session, ConversationEffect> for AnswerOperation {
    fn name(&self) -> &'static str {
        "answer"
    }

    async fn run(
        &self,
        session: &Session,
        conversation: &Conversation,
        _emit: &Emitter,
    ) -> Result<OperationResult<ConversationEffect>> {
        let calls = pending(conversation);
        let Some(request) = calls.iter().find(|request| is_answer(request)) else {
            return not_applicable();
        };
        let text = request
            .tool_call
            .as_ref()
            .ok()
            .and_then(|call| call.arguments.clone())
            .and_then(|arguments| {
                arguments
                    .get("text")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default();

        session.record.lock().unwrap().answer = Some(text);
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

/// The model answered in prose without calling `answer`. Accept it: refusing
/// would discard a perfectly good answer over a protocol detail.
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
        if record.answer.is_none() {
            let text = conversation
                .messages()
                .last()
                .map(|message| message.as_concat_text())
                .unwrap_or_default();
            record.answer = Some(text.trim().to_string());
        }
        Ok(OperationResult::Applied(StepResult {
            effects: Vec::new(),
            applied_step: None,
            yield_to_client: true,
        }))
    }
}

struct ToolExecutionOperation;

#[async_trait]
impl Operation<Session, ConversationEffect> for ToolExecutionOperation {
    fn name(&self) -> &'static str {
        "tool_execution"
    }

    async fn inference_tools(&self, _session: &Session) -> Result<Vec<Tool>> {
        Ok(tools::ask_definitions())
    }

    async fn run(
        &self,
        session: &Session,
        conversation: &Conversation,
        _emit: &Emitter,
    ) -> Result<OperationResult<ConversationEffect>> {
        let calls = pending(conversation);
        if calls.is_empty() || calls.iter().any(is_answer) {
            return not_applicable();
        }

        let mut response = Message::user();
        for request in calls {
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

            if name == "read_page" && !is_error {
                if let Some(path) = arguments.get("path").and_then(|v| v.as_str()) {
                    session
                        .record
                        .lock()
                        .unwrap()
                        .pages_read
                        .push(path.to_string());
                }
            }

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
        let next = session.record.lock().unwrap().turns + 1;
        session.reporter.thinking(next);

        let (reply, usage) = self
            .backend
            .complete(SYSTEM, conversation.messages(), &input.tools)
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

pub struct Asker {
    backend: Arc<dyn Backend>,
    pub max_turns: usize,
    pub reporter: Arc<Reporter>,
}

impl Asker {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self {
            backend,
            max_turns: DEFAULT_MAX_TURNS,
            reporter: Arc::new(Reporter::quiet()),
        }
    }

    pub fn ask(&self, home: &Path, wiki: &Path, question: &str) -> Result<Answer> {
        // Read-only, and the write tools are not offered. Two independent
        // reasons a question cannot modify the wiki.
        let mut workspace = Workspace::new(wiki)?;
        workspace.writable = false;
        workspace.home = Some(home.to_path_buf());

        let record = Arc::new(Mutex::new(Record {
            usage: Usage::default(),
            turns: 0,
            answer: None,
            pages_read: Vec::new(),
        }));

        let runtime = Runtime {
            conversation: Mutex::new(Conversation::new_unvalidated([Message::user().with_text(
                format!(
                    "Question: {question}\n\nSearch the wiki, read what looks relevant, \
                     then answer."
                ),
            )])),
            workspace: Arc::new(Mutex::new(workspace)),
            record: record.clone(),
            reporter: self.reporter.clone(),
        };

        let steps: Vec<Step<'_, Session, ConversationEffect>> = vec![
            Step::Operation(Arc::new(MaxTurnsOperation {
                limit: self.max_turns,
            })),
            Step::Operation(Arc::new(AnswerOperation)),
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

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async {
                let outcome = machine.run(&runtime, "ask", &emitter).await;
                rx.close();
                while rx.recv().await.is_some() {}
                outcome
            })?;

        let record = record.lock().unwrap();
        Ok(Answer {
            text: record
                .answer
                .clone()
                .unwrap_or_else(|| "(no answer)".to_string()),
            turns: record.turns,
            usage: record.usage,
            pages_read: record.pages_read.clone(),
        })
    }
}

pub const SYSTEM: &str = r#"You answer questions about the life of this wiki's owner, using only what the
wiki says.

The wiki is derived from an append-only log the owner wrote themselves. Pages
carry YAML frontmatter with `sources` listing the log entries behind their
claims, and footnotes like [^eNE004X79XW] attributing individual statements.

HOW TO WORK:
1. Search for the people, places and topics in the question. Search more than
   once, with different words, before concluding something is absent — the wiki
   may phrase it differently than the question does.
2. Read the pages that look relevant, including ones the first page links to.
3. Answer in plain prose, briefly. Say which pages you used.

HONESTY IS THE WHOLE POINT. This wiki is someone's memory of their own life, so
a confident wrong answer is far worse than no answer:
  - If the wiki does not say, say so plainly. Do not infer from adjacent facts.
  - If it says something ambiguous or contradictory, report the ambiguity rather
    than choosing.
  - If a fact carries a date, include it: "as of August 2026" may matter.
  - Never add knowledge from outside the wiki, even if you are confident it is
    true in general. You are reporting one person's record, not world facts.

Use `read_event` when the owner's exact words matter more than a page's summary,
for example when they asked what they *said* about something.

Call `answer` when you can answer, or to explain that the wiki does not cover
it."#;

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
    fn answers_from_the_wiki() {
        let dir = wiki();
        let backend = ScriptedBackend::new(vec![
            ScriptedBackend::tool_call("search_wiki", serde_json::json!({"query": "Ada"})),
            ScriptedBackend::tool_call("read_page", serde_json::json!({"path": "/people/ada.md"})),
            ScriptedBackend::tool_call(
                "answer",
                serde_json::json!({"text": "Ada lives in Porto, per /people/ada.md."}),
            ),
        ]);

        let answer = Asker::new(backend)
            .ask(dir.path(), dir.path(), "Where does Ada live?")
            .unwrap();

        assert!(answer.text.contains("Porto"), "{}", answer.text);
        assert_eq!(answer.pages_read, vec!["/people/ada.md"]);
        assert_eq!(answer.turns, 3);
    }

    /// The guarantee that matters: a question cannot change the wiki.
    #[test]
    fn cannot_write_to_the_wiki() {
        let dir = wiki();
        let before = std::fs::read_to_string(dir.path().join("people/ada.md")).unwrap();

        let backend = ScriptedBackend::new(vec![
            ScriptedBackend::tool_call(
                "write_page",
                serde_json::json!({
                    "path": "/people/ada.md",
                    "content": "---\ntype: Person\n---\n\nVandalised.\n"
                }),
            ),
            ScriptedBackend::tool_call("answer", serde_json::json!({"text": "done"})),
        ]);

        Asker::new(backend)
            .ask(dir.path(), dir.path(), "anything")
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("people/ada.md")).unwrap(),
            before,
            "ask must never modify a page"
        );
    }

    #[test]
    fn write_tools_are_not_even_offered() {
        let names: Vec<String> = tools::ask_definitions()
            .iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert!(names.contains(&"read_page".to_string()));
        assert!(names.contains(&"read_event".to_string()));
        assert!(!names.contains(&"write_page".to_string()));
        assert!(!names.contains(&"delete_page".to_string()));
    }

    #[test]
    fn prose_without_calling_answer_is_still_an_answer() {
        let dir = wiki();
        let backend = ScriptedBackend::new(vec![ScriptedBackend::text("The wiki does not say.")]);
        let answer = Asker::new(backend)
            .ask(dir.path(), dir.path(), "What is Ada's phone number?")
            .unwrap();
        assert!(answer.text.contains("does not say"), "{}", answer.text);
    }

    #[test]
    fn gives_up_rather_than_looping() {
        let dir = wiki();
        let backend = ScriptedBackend::new(
            (0..10)
                .map(|_| ScriptedBackend::tool_call("list_pages", serde_json::json!({})))
                .collect(),
        );
        let mut asker = Asker::new(backend);
        asker.max_turns = 3;
        let error = asker
            .ask(dir.path(), dir.path(), "anything")
            .unwrap_err()
            .to_string();
        assert!(error.contains("3 turns"), "{error}");
    }
}

//! The reconciliation agent.
//!
//! Given a batch of new log events, the agent explores the wiki with tools and
//! rewrites whatever the events contradict or extend. There is no fixed
//! retrieve-then-rewrite pipeline: search, read and write are tools, and the
//! agent decides how to use them.
//!
//! The loop is deliberately bounded. An agent with write access to your wiki
//! that can loop indefinitely is a way to burn money and corrupt pages, so
//! every run has a hard turn limit and every write is validated first.

use crate::event::Event;
use crate::model::{Conversation, Model, Usage};
use crate::tools::{self, Workspace};
use anyhow::{bail, Result};

pub const DEFAULT_MAX_TURNS: usize = 24;

#[derive(Debug)]
pub struct Outcome {
    pub summary: String,
    pub turns: usize,
    pub usage: Usage,
    pub journal: tools::Journal,
    /// Full transcript for the run log.
    pub transcript: Vec<String>,
}

pub struct Reconciler<'a> {
    pub model: &'a dyn Model,
    pub max_turns: usize,
}

impl<'a> Reconciler<'a> {
    pub fn new(model: &'a dyn Model) -> Self {
        Self {
            model,
            max_turns: DEFAULT_MAX_TURNS,
        }
    }

    pub fn run(&self, workspace: &mut Workspace, events: &[Event]) -> Result<Outcome> {
        if events.is_empty() {
            bail!("no events to reconcile");
        }
        let definitions = tools::definitions();
        let mut conversation = Conversation::new();
        conversation.push_user(kickoff(events));

        let mut transcript = Vec::new();
        let mut usage = Usage::default();
        let mut summary = String::new();

        for turn_number in 1..=self.max_turns {
            let turn = self.model.turn(SYSTEM, &mut conversation, &definitions)?;
            accumulate(&mut usage, turn.usage);

            if !turn.text.trim().is_empty() {
                transcript.push(format!("[{turn_number}] {}", turn.text.trim()));
            }

            if turn.calls.is_empty() {
                // No tool calls and no explicit finish: treat the text as the
                // summary and stop, rather than prompting in a loop.
                summary = turn.text.trim().to_string();
                return Ok(Outcome {
                    summary,
                    turns: turn_number,
                    usage,
                    journal: workspace.journal.clone(),
                    transcript,
                });
            }

            for call in &turn.calls {
                if call.name == "finish" {
                    summary = call
                        .arguments
                        .get("summary")
                        .and_then(|value| value.as_str())
                        .unwrap_or("(no summary given)")
                        .to_string();
                    transcript.push(format!("[{turn_number}] finish: {summary}"));
                    return Ok(Outcome {
                        summary,
                        turns: turn_number,
                        usage,
                        journal: workspace.journal.clone(),
                        transcript,
                    });
                }

                let (output, is_error) = tools::dispatch(workspace, &call.name, &call.arguments);
                transcript.push(format!(
                    "[{turn_number}] {}({}) -> {}{}",
                    call.name,
                    compact(&call.arguments),
                    if is_error { "ERROR " } else { "" },
                    first_line(&output)
                ));
                conversation.push_tool_result(&call.id, &output, is_error);
            }
        }

        bail!(
            "reconciliation did not finish within {} turns; \
             the wiki may be partially updated (run `git checkout -- wiki/` to discard)",
            self.max_turns
        )
    }
}

fn accumulate(total: &mut Usage, turn: Usage) {
    if let Some(input) = turn.input_tokens {
        *total.input_tokens.get_or_insert(0) += input;
    }
    if let Some(output) = turn.output_tokens {
        *total.output_tokens.get_or_insert(0) += output;
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

fn kickoff(events: &[Event]) -> String {
    let mut text =
        String::from("Fold these new log events into the wiki. Search before you write.\n\n");
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
    use crate::model::{Request, Response, ToolCall, Turn};
    use std::cell::RefCell;
    use std::sync::Mutex;

    /// A model that replays a fixed script of turns.
    struct ScriptedModel {
        turns: Mutex<RefCell<Vec<Turn>>>,
    }

    impl ScriptedModel {
        fn new(turns: Vec<Turn>) -> Self {
            Self {
                turns: Mutex::new(RefCell::new(turns)),
            }
        }
    }

    impl Model for ScriptedModel {
        fn actor(&self) -> String {
            "lifewiki/test".into()
        }
        fn complete(&self, _request: Request) -> Result<Response> {
            unimplemented!()
        }
        fn turn(
            &self,
            _system: &str,
            _history: &mut Conversation,
            _tools: &[rmcp::model::Tool],
        ) -> Result<Turn> {
            let guard = self.turns.lock().unwrap();
            let mut turns = guard.borrow_mut();
            if turns.is_empty() {
                bail!("script exhausted");
            }
            Ok(turns.remove(0))
        }
    }

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: format!("call_{name}"),
            name: name.to_string(),
            arguments,
        }
    }

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
        let mut workspace = Workspace::new(dir.path()).unwrap();
        let model = ScriptedModel::new(vec![
            Turn {
                calls: vec![call("search_wiki", serde_json::json!({"query": "apples"}))],
                ..Default::default()
            },
            Turn {
                calls: vec![call(
                    "write_page",
                    serde_json::json!({
                        "path": "/opinions/food.md",
                        "content": "---\ntype: Preference\n---\n\nPrefers apples.\n"
                    }),
                )],
                ..Default::default()
            },
            Turn {
                calls: vec![call(
                    "finish",
                    serde_json::json!({"summary": "Recorded fruit preference."}),
                )],
                ..Default::default()
            },
        ]);

        let outcome = Reconciler::new(&model)
            .run(&mut workspace, &[event("I prefer apples")])
            .unwrap();

        assert_eq!(outcome.summary, "Recorded fruit preference.");
        assert_eq!(outcome.journal.writes, vec!["/opinions/food.md"]);
        assert!(dir.path().join("opinions/food.md").exists());
    }

    #[test]
    fn invalid_page_is_reported_to_the_model_not_written() {
        let dir = scratch();
        let mut workspace = Workspace::new(dir.path()).unwrap();
        let model = ScriptedModel::new(vec![
            Turn {
                calls: vec![call(
                    "write_page",
                    serde_json::json!({"path": "/a.md", "content": "no frontmatter"}),
                )],
                ..Default::default()
            },
            Turn {
                calls: vec![call("finish", serde_json::json!({"summary": "gave up"}))],
                ..Default::default()
            },
        ]);

        let outcome = Reconciler::new(&model)
            .run(&mut workspace, &[event("something")])
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
        let mut workspace = Workspace::new(dir.path()).unwrap();
        let turns = (0..10)
            .map(|_| Turn {
                calls: vec![call("list_pages", serde_json::json!({}))],
                ..Default::default()
            })
            .collect();
        let model = ScriptedModel::new(turns);

        let mut reconciler = Reconciler::new(&model);
        reconciler.max_turns = 3;
        let error = reconciler
            .run(&mut workspace, &[event("x")])
            .unwrap_err()
            .to_string();
        assert!(error.contains("within 3 turns"), "{error}");
    }

    #[test]
    fn plain_text_reply_ends_the_run() {
        let dir = scratch();
        let mut workspace = Workspace::new(dir.path()).unwrap();
        let model = ScriptedModel::new(vec![Turn {
            text: "Nothing to change.".into(),
            ..Default::default()
        }]);
        let outcome = Reconciler::new(&model)
            .run(&mut workspace, &[event("x")])
            .unwrap();
        assert_eq!(outcome.summary, "Nothing to change.");
        assert_eq!(outcome.turns, 1);
    }
}

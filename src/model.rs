//! Model access for the reconciliation agent.
//!
//! The agent talks to this module's `Backend` trait, never to a provider
//! directly. That keeps the pipeline testable without an API key (see
//! `tests::ScriptedBackend`) and keeps provider churn — these crates are
//! `0.1.0-alpha` — contained to this file.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use goose_providers::anthropic::{AnthropicProviderBuilder, ANTHROPIC_API_VERSION};
use goose_providers::api_client::{ApiClient, AuthMethod};
use goose_providers::base::Provider;
use goose_providers::conversation::message::Message;
use goose_providers::model::ModelConfig;
use rmcp::model::Tool;
use std::sync::Arc;
use std::time::Duration;

/// What a model call cost, for run logs.
#[derive(Debug, Default, Clone, Copy)]
pub struct Usage {
    pub input_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
}

/// One inference call. Async because `goose_agent`'s `Inference` trait is.
#[async_trait]
pub trait Backend: Send + Sync {
    /// Actor string for OKF `generated.by`, e.g. `lifewiki/claude-sonnet-4-5`.
    fn actor(&self) -> String;

    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<(Message, Usage)>;
}

/// A backend that refuses to run, so "no model configured" is a clear error
/// rather than a network failure.
pub struct StubBackend;

#[async_trait]
impl Backend for StubBackend {
    fn actor(&self) -> String {
        "lifewiki/none".to_string()
    }

    async fn complete(
        &self,
        _system: &str,
        _messages: &[Message],
        _tools: &[Tool],
    ) -> Result<(Message, Usage)> {
        bail!(
            "no model is configured, so the wiki cannot be built.\n\
             Set [model] provider = \"anthropic\" in config.toml and export ANTHROPIC_API_KEY."
        )
    }
}

/// Anthropic via `goose-providers`.
pub struct AnthropicBackend {
    provider: goose_providers::anthropic::AnthropicProvider,
    config: ModelConfig,
    model_name: String,
}

impl AnthropicBackend {
    pub fn new(model_name: &str, timeout: Duration) -> Result<Self> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
            .context(
                "ANTHROPIC_API_KEY is not set (or is empty).\n\
                 Export it, or set provider = \"none\" in config.toml to disable model use.",
            )?;
        let api_client = ApiClient::with_timeout_and_tls(
            "https://api.anthropic.com".to_string(),
            AuthMethod::ApiKey {
                header_name: "x-api-key".to_string(),
                key,
            },
            timeout,
            None,
        )?
        .with_header("anthropic-version", ANTHROPIC_API_VERSION)?;

        Ok(Self {
            provider: AnthropicProviderBuilder::new(api_client).build(),
            config: ModelConfig::new(model_name).with_canonical_limits("anthropic"),
            model_name: model_name.to_string(),
        })
    }
}

#[async_trait]
impl Backend for AnthropicBackend {
    fn actor(&self) -> String {
        format!("lifewiki/{}", self.model_name.replace('.', "-"))
    }

    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<(Message, Usage)> {
        let (reply, usage) = self
            .provider
            .complete(&self.config, system, messages, tools)
            .await
            .context("model request failed")?;
        Ok((
            reply,
            Usage {
                input_tokens: usage.usage.input_tokens,
                output_tokens: usage.usage.output_tokens,
            },
        ))
    }
}

/// `[model]` section of config.toml.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ModelSettings {
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default = "default_model_name")]
    pub name: String,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

fn default_provider() -> String {
    "anthropic".to_string()
}
fn default_model_name() -> String {
    "claude-sonnet-4-5".to_string()
}
fn default_timeout() -> u64 {
    120
}

impl Default for ModelSettings {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            name: default_model_name(),
            timeout_seconds: default_timeout(),
        }
    }
}

#[derive(Debug, Default, serde::Deserialize)]
struct ConfigFile {
    #[serde(default)]
    model: ModelSettings,
}

impl ModelSettings {
    pub fn load(home: &std::path::Path) -> Result<Self> {
        let path = home.join("config.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let parsed: ConfigFile = toml::from_str(&raw)
            .with_context(|| format!("{} is not valid TOML", path.display()))?;
        Ok(parsed.model)
    }
}

pub fn build(settings: &ModelSettings) -> Result<Arc<dyn Backend>> {
    let timeout = Duration::from_secs(settings.timeout_seconds);
    match settings.provider.as_str() {
        "anthropic" => Ok(Arc::new(AnthropicBackend::new(&settings.name, timeout)?)),
        "none" | "stub" => Ok(Arc::new(StubBackend)),
        other => bail!("unknown model provider `{other}`; expected `anthropic` or `none`"),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use rmcp::model::CallToolRequestParams;
    use std::sync::Mutex;

    /// Replays a fixed script of assistant messages, and records what it was
    /// asked, so tests can assert the loop feeds tool results back.
    pub struct ScriptedBackend {
        replies: Mutex<Vec<Message>>,
        seen: Mutex<Vec<Message>>,
    }

    impl ScriptedBackend {
        pub fn new(replies: Vec<Message>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies),
                seen: Mutex::new(Vec::new()),
            })
        }

        /// An assistant message calling one tool.
        pub fn tool_call(name: &str, arguments: serde_json::Value) -> Message {
            let mut params = CallToolRequestParams::new(name.to_string());
            if let serde_json::Value::Object(map) = arguments {
                params = params.with_arguments(map);
            }
            Message::assistant().with_tool_request(format!("call_{name}"), Ok(params))
        }

        pub fn text(text: &str) -> Message {
            Message::assistant().with_text(text)
        }

        /// The messages passed to the most recent `complete` call.
        pub fn last_messages(&self) -> Vec<Message> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Backend for ScriptedBackend {
        fn actor(&self) -> String {
            "lifewiki/test".into()
        }

        async fn complete(
            &self,
            _system: &str,
            messages: &[Message],
            _tools: &[Tool],
        ) -> Result<(Message, Usage)> {
            *self.seen.lock().unwrap() = messages.to_vec();
            let mut replies = self.replies.lock().unwrap();
            if replies.is_empty() {
                bail!("script exhausted");
            }
            Ok((replies.remove(0), Usage::default()))
        }
    }

    #[tokio::test]
    async fn stub_refuses_and_says_why() {
        let error = StubBackend
            .complete("s", &[], &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("no model is configured"), "{error}");
        assert!(error.contains("config.toml"), "{error}");
    }

    #[test]
    fn actor_is_okf_shaped() {
        assert_eq!(StubBackend.actor(), "lifewiki/none");
    }

    /// Real network call. Ignored by default; run with:
    ///   cargo test -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn anthropic_round_trip() {
        let backend = AnthropicBackend::new("claude-sonnet-4-5", Duration::from_secs(60)).unwrap();
        let (reply, usage) = backend
            .complete(
                "Reply with exactly one word.",
                &[Message::user().with_text("Say the word: apples")],
                &[],
            )
            .await
            .unwrap();
        println!(
            "actor={} text={:?}",
            backend.actor(),
            reply.as_concat_text()
        );
        println!("usage={usage:?}");
        assert!(reply.as_concat_text().to_lowercase().contains("apples"));
    }
}

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

/// Local, offline inference via llama.cpp.
///
/// Behind the `local` feature because it compiles llama.cpp from source, which
/// is a slow build and irrelevant to anyone using a hosted provider. Worth the
/// trouble for this project specifically: the input is the user's entire life,
/// and this is the option where none of it leaves the machine.
#[cfg(feature = "local")]
pub struct LocalBackend {
    provider: goose_local_inference::LocalInferenceProvider,
    config: ModelConfig,
    model_name: String,
}

#[cfg(feature = "local")]
impl LocalBackend {
    pub async fn new(model_name: &str) -> Result<Self> {
        let provider = goose_local_inference::LocalInferenceProvider::from_env()
            .await
            .context(
                "could not start local inference.\n\
                 The model is downloaded on first use and may take a while.",
            )?;
        Ok(Self {
            provider,
            config: ModelConfig::new(model_name),
            model_name: model_name.to_string(),
        })
    }
}

#[cfg(feature = "local")]
#[async_trait]
impl Backend for LocalBackend {
    fn actor(&self) -> String {
        // Keep the local/ prefix: a page's provenance should make it obvious
        // which pages were written by a small local model.
        format!(
            "lifewiki/local-{}",
            self.model_name
                .rsplit('/')
                .next()
                .unwrap_or(&self.model_name)
                .replace([':', '.'], "-")
        )
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
            .map_err(|error| {
                // The upstream message points at goose's desktop UI, which
                // doesn't exist here. Downloading models is goose's job, not
                // ours, so point at its CLI instead.
                let text = error.to_string();
                if text.contains("not downloaded") || text.contains("Model not found") {
                    anyhow::anyhow!(
                        "local model `{}` is not downloaded.\n\
                         Fetch it with goose, then retry:\n  \
                         goose configure    # choose Local Inference and this model",
                        self.model_name
                    )
                } else {
                    anyhow::Error::from(error).context("local inference failed")
                }
            })?;
        Ok((
            reply,
            Usage {
                input_tokens: usage.usage.input_tokens,
                output_tokens: usage.usage.output_tokens,
            },
        ))
    }
}

/// Default local model. Small enough to run on a laptop; see the note in
/// `docs/pipeline.md` about tool-calling quality.
#[cfg(feature = "local")]
pub const DEFAULT_LOCAL_MODEL: &str = "unsloth/gemma-4-26B-A4B-it-GGUF:Q4_K_M";

/// Which agent a model is being built for.
///
/// The two tasks want different things, so they are configured separately.
/// Reconciliation rewrites files and must sustain many well-formed tool calls
/// over a long run, which rewards a stronger model. Answering is short, read-only
/// and much more frequent, so it is the sensible place to spend less — or to keep
/// entirely local, since a question and its answer are as sensitive as the wiki.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Task {
    Build,
    Ask,
}

impl Task {
    fn name(self) -> &'static str {
        match self {
            Task::Build => "build",
            Task::Ask => "ask",
        }
    }
}

/// Resolved settings for one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSettings {
    pub provider: String,
    pub name: String,
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

/// One `[model]` table or sub-table as written. Every field is optional so a
/// sub-table can override only what it cares about.
#[derive(Debug, Default, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelTable {
    provider: Option<String>,
    name: Option<String>,
    timeout_seconds: Option<u64>,
    /// Overrides for `lw build` / `lw rebuild`.
    #[serde(default)]
    build: Option<Box<ModelTable>>,
    /// Overrides for `lw ask`.
    #[serde(default)]
    ask: Option<Box<ModelTable>>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ConfigFile {
    #[serde(default)]
    model: ModelTable,
}

impl ModelTable {
    /// Resolve for a task, letting the task's sub-table override the base.
    fn resolve(&self, task: Task) -> ModelSettings {
        let base_provider = self.provider.clone().unwrap_or_else(default_provider);
        let specific = match task {
            Task::Build => self.build.as_deref(),
            Task::Ask => self.ask.as_deref(),
        };

        let provider = specific
            .and_then(|table| table.provider.clone())
            .unwrap_or_else(|| base_provider.clone());

        // A model name means something only to the provider it was written for
        // (S47). So a base `name` is inherited only when the task uses the same
        // provider; otherwise the new provider picks its own default. Without
        // this, `[model] name = "claude-opus-5"` plus `[model.ask] provider =
        // "local"` would ask the local provider for claude-opus-5.
        let name = specific
            .and_then(|table| table.name.clone())
            .or_else(|| {
                if provider == base_provider {
                    self.name.clone()
                } else {
                    None
                }
            })
            .unwrap_or_default();

        let timeout_seconds = specific
            .and_then(|table| table.timeout_seconds)
            .or(self.timeout_seconds)
            .unwrap_or_else(default_timeout);

        ModelSettings {
            provider,
            name,
            timeout_seconds,
        }
    }
}

impl ModelSettings {
    /// Override the provider, e.g. from `--local`.
    ///
    /// The model name is always cleared, for the reason given in
    /// `ModelTable::resolve`.
    pub fn with_provider(&self, provider: &str) -> Self {
        Self {
            provider: provider.to_string(),
            name: String::new(),
            timeout_seconds: self.timeout_seconds,
        }
    }

    /// Load settings for a task from `<home>/config.toml`.
    pub fn load(home: &std::path::Path, task: Task) -> Result<Self> {
        let path = home.join("config.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let parsed: ConfigFile = toml::from_str(&raw).with_context(|| {
            format!(
                "{} is not valid TOML (or has an unknown key under [model])",
                path.display()
            )
        })?;
        Ok(parsed.model.resolve(task))
    }

    /// A short description for logs and errors, e.g. `anthropic/claude-opus-5`.
    pub fn describe(&self) -> String {
        if self.name.is_empty() {
            format!("{} (default model)", self.provider)
        } else {
            format!("{}/{}", self.provider, self.name)
        }
    }
}

pub fn build(settings: &ModelSettings) -> Result<Arc<dyn Backend>> {
    let timeout = Duration::from_secs(settings.timeout_seconds);
    match settings.provider.as_str() {
        "anthropic" => {
            let name = if settings.name.is_empty() {
                default_model_name()
            } else {
                settings.name.clone()
            };
            Ok(Arc::new(AnthropicBackend::new(&name, timeout)?))
        }

        #[cfg(feature = "local")]
        "local" => {
            let name = if settings.name.is_empty() {
                DEFAULT_LOCAL_MODEL.to_string()
            } else {
                settings.name.clone()
            };
            // from_env is async but build() is called from sync code, so spin a
            // short-lived runtime just to construct the provider.
            let backend = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(LocalBackend::new(&name))?;
            Ok(Arc::new(backend))
        }

        #[cfg(not(feature = "local"))]
        "local" => bail!(
            "this binary was built without local inference support.\n\
             Rebuild with `cargo install --path . --features local` \
             (or --features metal on Apple silicon, cuda, or vulkan)."
        ),

        "none" | "stub" => Ok(Arc::new(StubBackend)),
        other => bail!(
            "unknown model provider `{other}`; expected `anthropic`, `local`, or `none`"
        ),
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

    /// Guards against a silent misconfiguration: `goose-providers` defaults to
    /// no TLS backend, and without one every HTTPS request fails as a
    /// "could not connect" network error that looks like the user's fault.
    /// Constructing a client is enough to catch it — reqwest panics at build
    /// time when asked for TLS it wasn't compiled with.
    #[tokio::test]
    async fn https_is_supported() {
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .build()
            .expect("no TLS backend compiled in; check goose-providers features");
        // A HEAD to a known-good endpoint: any HTTP status proves TLS works.
        // Only the transport is under test, so a network failure is tolerated.
        if let Ok(response) = client
            .head("https://api.anthropic.com/v1/models")
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            assert!(response.status().as_u16() > 0);
        }
    }

    fn settings(toml: &str, task: Task) -> ModelSettings {
        let dir = crate::tools::tests::tempdir::Dir::new();
        std::fs::write(dir.path().join("config.toml"), toml).unwrap();
        ModelSettings::load(dir.path(), task).unwrap()
    }

    #[test]
    fn both_tasks_share_the_base_table_by_default() {
        let toml = "[model]\nprovider = \"anthropic\"\nname = \"claude-opus-5\"\n";
        assert_eq!(settings(toml, Task::Build).name, "claude-opus-5");
        assert_eq!(settings(toml, Task::Ask).name, "claude-opus-5");
    }

    #[test]
    fn a_task_can_override_just_the_model() {
        let toml = "[model]\nprovider = \"anthropic\"\nname = \"claude-sonnet-4-5\"\n\
                    [model.build]\nname = \"claude-opus-5\"\n";
        let build = settings(toml, Task::Build);
        let ask = settings(toml, Task::Ask);
        assert_eq!(build.name, "claude-opus-5");
        assert_eq!(build.provider, "anthropic", "provider still inherited");
        assert_eq!(ask.name, "claude-sonnet-4-5");
    }

    /// The point of the feature: answer locally, build with a hosted model.
    #[test]
    fn a_task_can_use_a_different_provider() {
        let toml = "[model]\nprovider = \"anthropic\"\nname = \"claude-opus-5\"\n\
                    [model.ask]\nprovider = \"local\"\n";
        let ask = settings(toml, Task::Ask);
        assert_eq!(ask.provider, "local");
        assert_eq!(
            ask.name, "",
            "a name written for anthropic must not follow the provider change"
        );
        assert_eq!(settings(toml, Task::Build).name, "claude-opus-5");
    }

    #[test]
    fn a_task_may_pin_a_model_for_its_own_provider() {
        let toml = "[model]\nprovider = \"anthropic\"\n\
                    [model.ask]\nprovider = \"local\"\nname = \"some/model:Q4\"\n";
        assert_eq!(settings(toml, Task::Ask).name, "some/model:Q4");
    }

    #[test]
    fn timeout_is_inherited_and_overridable() {
        let toml = "[model]\ntimeout_seconds = 300\n[model.ask]\ntimeout_seconds = 30\n";
        assert_eq!(settings(toml, Task::Build).timeout_seconds, 300);
        assert_eq!(settings(toml, Task::Ask).timeout_seconds, 30);
    }

    #[test]
    fn an_absent_config_still_works() {
        let dir = crate::tools::tests::tempdir::Dir::new();
        let loaded = ModelSettings::load(dir.path(), Task::Build).unwrap();
        assert_eq!(loaded, ModelSettings::default());
    }

    /// A typo in a task name would otherwise be silently ignored, leaving the
    /// user wondering why their override did nothing.
    #[test]
    fn an_unknown_key_is_an_error() {
        let dir = crate::tools::tests::tempdir::Dir::new();
        std::fs::write(
            dir.path().join("config.toml"),
            "[model]\n[model.asking]\nprovider = \"local\"\n",
        )
        .unwrap();
        let error = ModelSettings::load(dir.path(), Task::Ask)
            .unwrap_err()
            .to_string();
        assert!(error.contains("config.toml"), "{error}");
    }

    #[test]
    fn describe_names_the_provider_and_model() {
        let settings = ModelSettings {
            provider: "local".into(),
            name: String::new(),
            timeout_seconds: 60,
        };
        assert_eq!(settings.describe(), "local (default model)");
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

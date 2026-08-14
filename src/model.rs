//! Model access for the reconciliation pipeline.
//!
//! The pipeline talks to this module's `Model` trait, never to a provider
//! directly. That keeps the pipeline testable without an API key (see
//! `StubModel`) and keeps provider churn — these crates are 0.1.0-alpha —
//! contained to this file.

use anyhow::{bail, Context, Result};
use goose_providers::anthropic::{AnthropicProviderBuilder, ANTHROPIC_API_VERSION};
use goose_providers::api_client::{ApiClient, AuthMethod};
use goose_providers::base::Provider;
use goose_providers::conversation::message::Message;
use goose_providers::model::ModelConfig;
use std::time::Duration;

/// One completion request. Deliberately minimal: the pipeline sends a system
/// prompt plus a single user turn and expects text back. No tools, no
/// multi-turn state — reconciliation is a pure function of (event, pages).
pub struct Request {
    pub system: String,
    pub user: String,
}

/// What a model call cost, for run logs.
#[derive(Debug, Default, Clone, Copy)]
pub struct Usage {
    pub input_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
}

#[derive(Debug)]
pub struct Response {
    pub text: String,
    pub usage: Usage,
}

pub trait Model: Send + Sync {
    /// Actor string for OKF `generated.by`, e.g. `lifewiki/claude-sonnet-4-5`.
    fn actor(&self) -> String;
    fn complete(&self, request: Request) -> Result<Response>;
}

/// A model that refuses to run. Used by the deterministic pipeline steps so
/// that "no model configured" is only an error if something actually needs one.
pub struct StubModel;

impl Model for StubModel {
    fn actor(&self) -> String {
        "lifewiki/deterministic".to_string()
    }
    fn complete(&self, _request: Request) -> Result<Response> {
        bail!("this operation requires a model; configure one in config.toml")
    }
}

/// Anthropic via `goose-providers`.
pub struct AnthropicModel {
    provider: goose_providers::anthropic::AnthropicProvider,
    config: ModelConfig,
    model_name: String,
    runtime: tokio::runtime::Runtime,
}

impl AnthropicModel {
    pub fn new(model_name: &str, timeout: Duration) -> Result<Self> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .context("ANTHROPIC_API_KEY is not set; export it or use a local provider")?;
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

        let provider = AnthropicProviderBuilder::new(api_client).build();
        let config = ModelConfig::new(model_name).with_canonical_limits("anthropic");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("could not start async runtime")?;

        Ok(Self {
            provider,
            config,
            model_name: model_name.to_string(),
            runtime,
        })
    }
}

impl Model for AnthropicModel {
    fn actor(&self) -> String {
        format!("lifewiki/{}", self.model_name.replace('.', "-"))
    }

    fn complete(&self, request: Request) -> Result<Response> {
        let message = Message::user().with_text(request.user);
        let (reply, usage) = self
            .runtime
            .block_on(async {
                self.provider
                    .complete(&self.config, &request.system, &[message], &[])
                    .await
            })
            .context("model request failed")?;

        let text = reply.as_concat_text();
        if text.trim().is_empty() {
            bail!("model returned an empty response");
        }
        Ok(Response {
            text,
            usage: Usage {
                input_tokens: usage.usage.input_tokens,
                output_tokens: usage.usage.output_tokens,
            },
        })
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

/// Build the configured model. Errors here should be surfaced only when a
/// model is actually required, so callers that can work deterministically
/// should not call this at all.
pub fn build(settings: &ModelSettings) -> Result<Box<dyn Model>> {
    let timeout = Duration::from_secs(settings.timeout_seconds);
    match settings.provider.as_str() {
        "anthropic" => Ok(Box::new(AnthropicModel::new(&settings.name, timeout)?)),
        "none" | "stub" => Ok(Box::new(StubModel)),
        other => bail!("unknown model provider `{other}`; expected `anthropic` or `none`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_refuses_and_says_why() {
        let error = StubModel
            .complete(Request {
                system: "s".into(),
                user: "u".into(),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("requires a model"), "{error}");
    }

    #[test]
    fn actor_is_okf_shaped() {
        assert_eq!(StubModel.actor(), "lifewiki/deterministic");
    }

    /// Real network call. Ignored by default; run with:
    ///   cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn anthropic_round_trip() {
        let model = AnthropicModel::new("claude-sonnet-4-5", Duration::from_secs(60)).unwrap();
        let response = model
            .complete(Request {
                system: "Reply with exactly one word.".into(),
                user: "Say the word: apples".into(),
            })
            .unwrap();
        println!("actor={} text={:?}", model.actor(), response.text);
        println!("usage={:?}", response.usage);
        assert!(response.text.to_lowercase().contains("apples"));
    }
}

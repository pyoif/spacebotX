//! LLM manager for provider credentials and HTTP client.
//!
//! The manager is intentionally simple — it holds API keys, an HTTP client,
//! and shared rate limit state. Routing decisions (which model for which
//! process) live on the agent's RoutingConfig, not here.
//!
//! API keys are hot-reloadable via ArcSwap. The file watcher calls
//! `reload_config()` when config.toml changes, and all subsequent
//! `get_api_key()` calls read the new values lock-free.

use crate::auth::OAuthCredentials as AnthropicOAuthCredentials;
use crate::config::{ApiType, LlmConfig, ProviderConfig};
use crate::error::{LlmError, Result};
use crate::github_copilot_auth::CopilotToken;
use crate::openai_auth::OAuthCredentials as OpenAiOAuthCredentials;

use anyhow::Context as _;
use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::path::PathBuf;

/// Editor version header for GitHub Copilot API requests.
/// Matches VSCode 1.96.2 which Copilot expects for IDE auth.
const COPILOT_EDITOR_VERSION: &str = "vscode/1.96.2";

/// Editor plugin version header for GitHub Copilot API requests.
/// Matches Copilot Chat extension version 0.26.7.
const COPILOT_EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.26.7";
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

/// Manages LLM provider clients and tracks rate limit state.
pub struct LlmManager {
    config: ArcSwap<LlmConfig>,
    http_client: reqwest::Client,
    /// Models currently in rate limit cooldown, with the time they were limited.
    rate_limited: Arc<RwLock<HashMap<String, Instant>>>,
    /// Instance directory for reading/writing OAuth credentials.
    instance_dir: Option<PathBuf>,
    /// Cached Anthropic OAuth credentials (refreshed lazily).
    anthropic_oauth_credentials: RwLock<Option<AnthropicOAuthCredentials>>,
    /// Cached OpenAI OAuth credentials (refreshed lazily).
    openai_oauth_credentials: RwLock<Option<OpenAiOAuthCredentials>>,
    /// Cached GitHub Copilot API token (exchanged from PAT, refreshed lazily).
    copilot_token: RwLock<Option<CopilotToken>>,
    /// What each model's requests are allowed to grow to.
    ///
    /// Lives here because every `SpacebotModel` already shares this manager, so
    /// a ceiling learned by one run applies to the next without threading it
    /// through fifteen construction sites.
    context_ceilings: ArcSwap<ContextCeilings>,
}

/// What a request is allowed to grow to, per model.
///
/// A published context window is not what a backend enforces: the same model
/// answers to a different ceiling depending on which API it is reached through,
/// and that ceiling moves without notice. `default` is the configured fallback;
/// `learned` holds what a provider has demonstrated by refusing a request of
/// known size.
#[derive(Debug, Default, Clone)]
pub struct ContextCeilings {
    pub default: Option<usize>,
    pub learned: HashMap<String, usize>,
    /// Windows resolved from the provider's `/models` listing or from models.dev.
    /// Weaker evidence than a refusal, but far better than guessing.
    pub discovered: HashMap<String, usize>,
}

impl ContextCeilings {
    /// What this model's requests must fit inside, if anything is known.
    ///
    /// A refusal only ever tightens: it proves the ceiling sits below the size
    /// refused and says nothing about whether the configured window was too
    /// generous, so the smaller of the two is what a request has to fit.
    pub fn ceiling_for(&self, full_model_name: &str) -> Option<usize> {
        // A refusal is the strongest evidence, a published window is next, and
        // the configured default covers everything else. The configured value
        // stays an upper bound in every combination, so resolution can tighten
        // a window but never widen it past what was asked for.
        match (
            self.learned.get(full_model_name).copied(),
            self.discovered.get(full_model_name).copied(),
            self.default,
        ) {
            (Some(learned), Some(discovered), Some(default)) => {
                Some(learned.min(discovered).min(default))
            }
            (Some(learned), Some(discovered), None) => Some(learned.min(discovered)),
            (Some(learned), None, Some(default)) => Some(learned.min(default)),
            (Some(learned), None, None) => Some(learned),
            (None, Some(discovered), Some(default)) => Some(discovered.min(default)),
            (None, Some(discovered), None) => Some(discovered),
            (None, None, default) => default,
        }
    }

    /// Fold a window resolved from `/models` or models.dev into the ceilings.
    ///
    /// Unlike a refusal this is additive evidence, but recording it is still
    /// idempotent: the same value is not stored twice, and an implausible number
    /// is rejected outright so a junk lookup cannot poison the map.
    pub fn with_discovered(&self, full_model_name: &str, tokens: usize) -> Option<Self> {
        if !crate::llm::context_window::is_plausible_context_window(tokens) {
            return None;
        }
        if self.discovered.get(full_model_name).copied() == Some(tokens) {
            return None;
        }

        let mut discovered = self.discovered.clone();
        discovered.insert(full_model_name.to_string(), tokens);
        Some(Self {
            default: self.default,
            learned: self.learned.clone(),
            discovered,
        })
    }

    /// Fold a rejection of `estimated_tokens` into the ceilings.
    ///
    /// Returns `None` when nothing was learned: a rejection at or above what is
    /// already known says nothing new, so only a smaller one tightens the
    /// ceiling. Moving in one direction keeps a single unlucky large request
    /// from undoing a limit that was correctly discovered.
    pub fn with_overflow(&self, full_model_name: &str, estimated_tokens: usize) -> Option<Self> {
        // Back off from the refused size rather than sitting on the boundary,
        // since the estimate is approximate in both directions.
        let ceiling = estimated_tokens.saturating_mul(9) / 10;
        if ceiling == 0 {
            return None;
        }
        if self
            .ceiling_for(full_model_name)
            .is_some_and(|known| known <= ceiling)
        {
            return None;
        }

        let mut learned = self.learned.clone();
        learned.insert(full_model_name.to_string(), ceiling);
        Some(Self {
            default: self.default,
            learned,
            discovered: self.discovered.clone(),
        })
    }
}

impl LlmManager {
    /// Create a new LLM manager with the given configuration.
    pub async fn new(config: LlmConfig) -> Result<Self> {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .with_context(|| "failed to build HTTP client")?;

        Ok(Self {
            config: ArcSwap::from_pointee(config),
            http_client,
            rate_limited: Arc::new(RwLock::new(HashMap::new())),
            instance_dir: None,
            anthropic_oauth_credentials: RwLock::new(None),
            openai_oauth_credentials: RwLock::new(None),
            copilot_token: RwLock::new(None),
            context_ceilings: ArcSwap::from_pointee(ContextCeilings::default()),
        })
    }

    /// Set the instance directory and load any existing OAuth credentials.
    pub async fn set_instance_dir(&self, instance_dir: PathBuf) {
        if let Ok(Some(creds)) = crate::auth::load_credentials(&instance_dir) {
            tracing::info!("loaded Anthropic OAuth credentials from auth.json");
            *self.anthropic_oauth_credentials.write().await = Some(creds);
        }
        if let Ok(Some(creds)) = crate::openai_auth::load_credentials(&instance_dir) {
            tracing::info!("loaded OpenAI OAuth credentials from openai_chatgpt_oauth.json");
            *self.openai_oauth_credentials.write().await = Some(creds);
        }
        match crate::github_copilot_auth::load_cached_token(&instance_dir) {
            Ok(Some(token)) => {
                tracing::info!("loaded GitHub Copilot token from github_copilot_token.json");
                *self.copilot_token.write().await = Some(token);
            }
            Ok(None) => {
                tracing::debug!("no cached GitHub Copilot token found");
            }
            Err(error) => {
                tracing::warn!(%error, "failed to load GitHub Copilot token");
            }
        }
        // Store instance_dir — we can't set it on &self since it's not behind RwLock,
        // but we only need it for save_credentials which we handle inline.
    }

    /// Initialize with an instance directory (for use at construction time).
    pub async fn with_instance_dir(config: LlmConfig, instance_dir: PathBuf) -> Result<Self> {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .with_context(|| "failed to build HTTP client")?;

        let anthropic_oauth_credentials = match crate::auth::load_credentials(&instance_dir) {
            Ok(Some(creds)) => {
                tracing::info!("loaded Anthropic OAuth credentials from auth.json");
                Some(creds)
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(%error, "failed to load Anthropic OAuth credentials");
                None
            }
        };

        let openai_oauth_credentials = match crate::openai_auth::load_credentials(&instance_dir) {
            Ok(Some(creds)) => {
                tracing::info!("loaded OpenAI OAuth credentials from openai_chatgpt_oauth.json");
                Some(creds)
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(%error, "failed to load OpenAI OAuth credentials");
                None
            }
        };

        let copilot_token = match crate::github_copilot_auth::load_cached_token(&instance_dir) {
            Ok(Some(token)) => {
                tracing::info!("loaded GitHub Copilot token from github_copilot_token.json");
                Some(token)
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(%error, "failed to load GitHub Copilot token");
                None
            }
        };

        Ok(Self {
            config: ArcSwap::from_pointee(config),
            http_client,
            rate_limited: Arc::new(RwLock::new(HashMap::new())),
            instance_dir: Some(instance_dir),
            anthropic_oauth_credentials: RwLock::new(anthropic_oauth_credentials),
            openai_oauth_credentials: RwLock::new(openai_oauth_credentials),
            copilot_token: RwLock::new(copilot_token),
            context_ceilings: ArcSwap::from_pointee(ContextCeilings::default()),
        })
    }

    /// The configured fallback ceiling, applied to any model with nothing learned.
    ///
    /// Read-modify-write under `rcu`: a refusal recorded by a request in flight
    /// must not be dropped by this write, and vice versa.
    pub fn set_default_context_ceiling(&self, tokens: usize) {
        self.context_ceilings.rcu(|current| ContextCeilings {
            default: Some(tokens),
            learned: current.learned.clone(),
            discovered: current.discovered.clone(),
        });
    }

    /// What this model's requests must fit inside, if anything is known.
    pub fn context_ceiling(&self, full_model_name: &str) -> Option<usize> {
        self.context_ceilings.load().ceiling_for(full_model_name)
    }

    /// Whether this model's context window has already been resolved.
    ///
    /// The caller uses this to skip the lookup entirely: once a window is known
    /// the resolution never has to run again for that model.
    pub fn context_window_discovered(&self, full_model_name: &str) -> Option<usize> {
        self.context_ceilings
            .load()
            .discovered
            .get(full_model_name)
            .copied()
    }

    /// Record a window resolved from the provider's `/models` listing or from
    /// models.dev.
    ///
    /// Read-modify-write under `rcu`, so a refusal recorded concurrently cannot
    /// be dropped, and an implausible value is refused by `with_discovered`.
    pub fn note_resolved_context_window(&self, full_model_name: &str, tokens: usize) {
        self.context_ceilings.rcu(|current| {
            current
                .with_discovered(full_model_name, tokens)
                .unwrap_or_else(|| (**current).clone())
        });
    }

    /// Where the cached models.dev payload lives, next to other agent state.
    ///
    /// The name changed with the switch from `api.json` to the flat `models.json`
    /// listing, so a payload cached in the old provider-nested shape is not read
    /// back and the first lookup after an upgrade fetches the new source.
    ///
    /// `None` when the manager has no instance directory, in which case the
    /// models.dev lookup is simply skipped and the configured default stands.
    pub fn models_dev_cache_path(&self) -> Option<PathBuf> {
        self.instance_dir
            .as_ref()
            .map(|dir| dir.join("models_dev_models.json"))
    }

    /// Record that a request of this size was refused for exceeding the window.
    ///
    /// The refusal is the only trustworthy measurement available: it proves the
    /// ceiling sits below `estimated_tokens`. Following the lowest observed
    /// refusal means a backend that silently tightens its limit is tracked
    /// rather than fought.
    /// Read-modify-write under `rcu`, so two models learning at once cannot
    /// drop each other's ceiling and a stale copy cannot widen a tighter one.
    /// The closure can run more than once, which is safe: `with_overflow` is a
    /// pure function of the state it is handed.
    pub fn note_context_overflow(&self, full_model_name: &str, estimated_tokens: usize) {
        let mut learned: Option<usize> = None;
        self.context_ceilings.rcu(|current| {
            match current.with_overflow(full_model_name, estimated_tokens) {
                Some(updated) => {
                    learned = updated.ceiling_for(full_model_name);
                    updated
                }
                None => {
                    learned = None;
                    (**current).clone()
                }
            }
        });
        let Some(ceiling) = learned else {
            return;
        };

        tracing::warn!(
            model = %full_model_name,
            rejected_at = estimated_tokens,
            ceiling,
            "provider refused a request for exceeding its context window; \
             lowering the ceiling for this model"
        );
    }

    /// Atomically swap in new provider credentials.
    pub fn reload_config(&self, config: LlmConfig) {
        self.config.store(Arc::new(config));
        tracing::info!("LLM provider keys reloaded");
    }

    pub fn get_provider(&self, provider_id: &str) -> Result<ProviderConfig> {
        let normalized_provider_id = provider_id.to_lowercase();
        let config = self.config.load();

        config
            .providers
            .get(&normalized_provider_id)
            .cloned()
            .ok_or_else(|| LlmError::UnknownProvider(provider_id.to_string()).into())
    }

    /// Get the appropriate API key for a provider, with OAuth override for Anthropic.
    ///
    /// If OAuth credentials are available and the provider is Anthropic,
    /// returns the OAuth access token (refreshing if needed). Otherwise
    /// falls back to the static API key from config.
    pub async fn get_anthropic_token(&self) -> Result<Option<String>> {
        let mut creds_guard = self.anthropic_oauth_credentials.write().await;
        let Some(creds) = creds_guard.as_ref() else {
            return Ok(None);
        };

        if !creds.is_expired() {
            return Ok(Some(creds.access_token.clone()));
        }

        // Need to refresh
        tracing::info!("Anthropic OAuth access token expired, refreshing...");
        match creds.refresh().await {
            Ok(new_creds) => {
                // Save to disk
                if let Some(ref instance_dir) = self.instance_dir
                    && let Err(error) = crate::auth::save_credentials(instance_dir, &new_creds)
                {
                    tracing::warn!(%error, "failed to persist refreshed Anthropic OAuth credentials");
                }
                let token = new_creds.access_token.clone();
                *creds_guard = Some(new_creds);
                tracing::info!("Anthropic OAuth token refreshed successfully");
                Ok(Some(token))
            }
            Err(error) => {
                tracing::error!(%error, "Anthropic OAuth token refresh failed");
                // Return the expired token anyway — the API will reject it
                // and the error message will be clearer than "no key"
                Ok(Some(creds.access_token.clone()))
            }
        }
    }

    /// Resolve the Anthropic provider config, preferring OAuth credentials.
    ///
    /// If a static provider exists in config, returns it with the API key
    /// overridden by the OAuth token when available. If no static provider
    /// exists but OAuth credentials are present, builds a provider from
    /// the OAuth token alone.
    pub async fn get_anthropic_provider(&self) -> Result<ProviderConfig> {
        let token = self.get_anthropic_token().await?;
        let static_provider = self.get_provider("anthropic").ok();

        match (static_provider, token) {
            (Some(mut provider), Some(token)) => {
                provider.api_key = token;
                Ok(provider)
            }
            (Some(provider), None) => Ok(provider),
            (None, Some(token)) => Ok(ProviderConfig {
                api_type: ApiType::Anthropic,
                base_url: "https://api.anthropic.com".to_string(),
                api_key: token,
                name: None,
                use_bearer_auth: false,
                extra_headers: vec![],
                api_version: None,
                deployment: None,
            }),
            (None, None) => Err(LlmError::UnknownProvider("anthropic".to_string()).into()),
        }
    }

    /// Set OpenAI OAuth credentials in memory after successful auth.
    pub async fn set_openai_oauth_credentials(&self, creds: OpenAiOAuthCredentials) {
        *self.openai_oauth_credentials.write().await = Some(creds);
    }

    /// Clear OpenAI OAuth credentials from memory.
    pub async fn clear_openai_oauth_credentials(&self) {
        *self.openai_oauth_credentials.write().await = None;
    }

    /// Get OpenAI OAuth access token if available, refreshing when needed.
    pub async fn get_openai_token(&self) -> Result<Option<String>> {
        let mut creds_guard = self.openai_oauth_credentials.write().await;
        let Some(creds) = creds_guard.as_ref() else {
            return Ok(None);
        };

        if !creds.is_expired() {
            return Ok(Some(creds.access_token.clone()));
        }

        tracing::info!("OpenAI OAuth access token expired, refreshing...");
        match creds.refresh().await {
            Ok(new_creds) => {
                if let Some(ref instance_dir) = self.instance_dir
                    && let Err(error) =
                        crate::openai_auth::save_credentials(instance_dir, &new_creds)
                {
                    tracing::warn!(%error, "failed to persist refreshed OpenAI OAuth credentials");
                }
                let token = new_creds.access_token.clone();
                *creds_guard = Some(new_creds);
                tracing::info!("OpenAI OAuth token refreshed successfully");
                Ok(Some(token))
            }
            Err(error) => {
                tracing::error!(%error, "OpenAI OAuth token refresh failed");
                Ok(Some(creds.access_token.clone()))
            }
        }
    }

    /// Resolve the OpenAI provider config from static API-key configuration.
    ///
    /// OpenAI ChatGPT OAuth is intentionally handled via a separate internal
    /// provider (`openai-chatgpt`) so a saved OAuth token cannot shadow a
    /// configured `openai` API key.
    pub async fn get_openai_provider(&self) -> Result<ProviderConfig> {
        self.get_provider("openai")
    }

    /// Resolve the OpenAI ChatGPT OAuth provider config.
    ///
    /// This internal provider uses OAuth access tokens from ChatGPT Plus/Pro.
    pub async fn get_openai_chatgpt_provider(&self) -> Result<ProviderConfig> {
        let token = self.get_openai_token().await?;

        match token {
            Some(token) => Ok(ProviderConfig {
                api_type: ApiType::OpenAiResponses,
                base_url: "https://chatgpt.com/backend-api/codex".to_string(),
                api_key: token,
                name: None,
                use_bearer_auth: false,
                extra_headers: vec![],
                api_version: None,
                deployment: None,
            }),
            None => Err(LlmError::UnknownProvider("openai-chatgpt".to_string()).into()),
        }
    }

    /// Get OpenAI OAuth account id (for ChatGPT Plus/Pro account scoping headers).
    pub async fn get_openai_account_id(&self) -> Option<String> {
        self.openai_oauth_credentials
            .read()
            .await
            .as_ref()
            .and_then(|credentials| credentials.account_id.clone())
    }

    /// Get a valid GitHub Copilot API token, exchanging/refreshing as needed.
    ///
    /// Reads the GitHub PAT from the `github-copilot` provider config, checks
    /// whether the cached Copilot token is still valid, and exchanges for a new
    /// one if expired or missing. Saves refreshed tokens to disk.
    pub async fn get_copilot_token(&self) -> Result<Option<String>> {
        // Check if there's a github-copilot provider configured with a PAT
        let github_pat = match self.get_provider("github-copilot") {
            Ok(provider) if !provider.api_key.is_empty() => provider.api_key,
            _ => return Ok(None),
        };

        let pat_hash = crate::github_copilot_auth::hash_pat(&github_pat);

        // Check cached token — must be unexpired AND for the same PAT
        {
            let token_guard = self.copilot_token.read().await;
            if let Some(ref cached) = *token_guard
                && !cached.is_expired()
                && cached.pat_hash == pat_hash
            {
                return Ok(Some(cached.token.clone()));
            }
        } // read lock dropped here before network call

        // Need to exchange
        tracing::info!("exchanging GitHub PAT for Copilot API token...");
        match crate::github_copilot_auth::exchange_github_token(
            &self.http_client,
            &github_pat,
            pat_hash.clone(),
        )
        .await
        {
            Ok(new_token) => {
                let api_token = new_token.token.clone();
                // Save to disk
                if let Some(ref instance_dir) = self.instance_dir
                    && let Err(error) =
                        crate::github_copilot_auth::save_cached_token(instance_dir, &new_token)
                {
                    tracing::warn!(%error, "failed to persist GitHub Copilot token");
                }
                // Update cache with write lock held only for the assignment
                *self.copilot_token.write().await = Some(new_token);
                tracing::info!("GitHub Copilot token exchanged successfully");
                Ok(Some(api_token))
            }
            Err(error) => {
                tracing::error!(%error, "GitHub Copilot token exchange failed");
                // Only fall back to cached token if it matches the current PAT hash
                let token_guard = self.copilot_token.read().await;
                if let Some(ref cached) = *token_guard
                    && cached.pat_hash == pat_hash
                {
                    return Ok(Some(cached.token.clone()));
                }
                Err(error.into())
            }
        }
    }

    /// Resolve the GitHub Copilot provider config with a fresh API token.
    ///
    /// Exchanges the stored GitHub PAT for a Copilot API token, derives the
    /// base URL from the token's `proxy-ep` field, and returns a complete
    /// `ProviderConfig` ready for OpenAI-compatible API calls.
    pub async fn get_github_copilot_provider(&self) -> Result<ProviderConfig> {
        let token = self
            .get_copilot_token()
            .await?
            .ok_or_else(|| LlmError::UnknownProvider("github-copilot".to_string()))?;

        let base_url = crate::github_copilot_auth::derive_base_url_from_token(&token)
            .unwrap_or_else(|| {
                crate::github_copilot_auth::DEFAULT_COPILOT_API_BASE_URL.to_string()
            });

        Ok(ProviderConfig {
            api_type: ApiType::OpenAiChatCompletions,
            base_url,
            api_key: token,
            name: Some("GitHub Copilot".to_string()),
            use_bearer_auth: true,
            extra_headers: vec![
                (
                    "user-agent".to_string(),
                    format!("spacebot/{}", env!("CARGO_PKG_VERSION")),
                ),
                (
                    "editor-version".to_string(),
                    COPILOT_EDITOR_VERSION.to_string(),
                ),
                (
                    "editor-plugin-version".to_string(),
                    COPILOT_EDITOR_PLUGIN_VERSION.to_string(),
                ),
            ],
            api_version: None,
            deployment: None,
        })
    }

    /// Clear cached GitHub Copilot token from memory only.
    ///
    /// Note: Does not delete the on-disk cache file. Use
    /// `github_copilot_auth::credentials_path()` and delete the file separately
    /// if persistent removal is needed (e.g., in `delete_provider`).
    pub async fn clear_copilot_token(&self) {
        *self.copilot_token.write().await = None;
    }

    /// Get the appropriate API key for a provider.
    pub fn get_api_key(&self, provider_id: &str) -> Result<String> {
        let provider = self.get_provider(provider_id)?;

        if provider.api_key.is_empty() {
            return Err(LlmError::MissingProviderKey(provider_id.to_string()).into());
        }

        Ok(provider.api_key)
    }

    /// Get configured Ollama base URL, if provided.
    pub fn ollama_base_url(&self) -> Option<String> {
        self.config.load().ollama_base_url.clone()
    }

    /// Get the HTTP client.
    pub fn http_client(&self) -> &reqwest::Client {
        &self.http_client
    }

    /// Resolve a model name to provider and model components.
    /// Format: "provider/model-name" or just "model-name" (defaults to anthropic).
    pub fn resolve_model(&self, model_name: &str) -> Result<(String, String)> {
        if let Some((provider, model)) = model_name.split_once('/') {
            Ok((provider.to_string(), model.to_string()))
        } else {
            Ok(("anthropic".into(), model_name.into()))
        }
    }

    /// Record that a model hit a rate limit.
    pub async fn record_rate_limit(&self, model_name: &str) {
        self.rate_limited
            .write()
            .await
            .insert(model_name.to_string(), Instant::now());
        tracing::warn!(model = %model_name, "model rate limited, entering cooldown");
    }

    /// Check if a model is currently in rate limit cooldown.
    pub async fn is_rate_limited(&self, model_name: &str, cooldown_secs: u64) -> bool {
        let map = self.rate_limited.read().await;
        if let Some(limited_at) = map.get(model_name) {
            limited_at.elapsed().as_secs() < cooldown_secs
        } else {
            false
        }
    }

    /// Clean up expired rate limit entries.
    pub async fn cleanup_rate_limits(&self, cooldown_secs: u64) {
        self.rate_limited
            .write()
            .await
            .retain(|_, limited_at| limited_at.elapsed().as_secs() < cooldown_secs);
    }
}

#[cfg(test)]
mod context_ceiling_tests {
    use super::ContextCeilings;

    #[test]
    fn nothing_is_enforced_until_a_ceiling_is_known() {
        let ceilings = ContextCeilings::default();
        assert_eq!(ceilings.ceiling_for("openai-chatgpt/gpt-5.6-sol"), None);
    }

    #[test]
    fn the_configured_default_applies_to_every_model() {
        let ceilings = ContextCeilings {
            default: Some(128_000),
            ..Default::default()
        };
        assert_eq!(
            ceilings.ceiling_for("openai-chatgpt/gpt-5.6-sol"),
            Some(128_000)
        );
        assert_eq!(ceilings.ceiling_for("anything/else"), Some(128_000));
    }

    /// The case that killed two workers: the backend enforced far less than the
    /// model advertises, and the only way to find out was to be refused.
    #[test]
    fn a_refusal_teaches_the_ceiling_for_that_model_alone() {
        let ceilings = ContextCeilings {
            default: Some(1_050_000),
            ..Default::default()
        };

        let learned = ceilings
            .with_overflow("openai-chatgpt/gpt-5.6-sol", 257_963)
            .expect("a refusal teaches something");

        let ceiling = learned
            .ceiling_for("openai-chatgpt/gpt-5.6-sol")
            .expect("learned");
        assert!(
            ceiling < 257_963,
            "the ceiling must sit below the size that was refused"
        );
        assert_eq!(ceiling, 232_166);

        // Every other model keeps the configured default.
        assert_eq!(
            learned.ceiling_for("anthropic/claude-sonnet-4"),
            Some(1_050_000)
        );
    }

    /// A backend that tightens again must be followed down, and one that
    /// happens to refuse a larger request must not undo what was learned.
    #[test]
    fn the_ceiling_only_ever_moves_down() {
        let ceilings = ContextCeilings {
            default: Some(400_000),
            ..Default::default()
        };

        let first = ceilings.with_overflow("m", 300_000).expect("learned");
        let learned = first.ceiling_for("m").expect("learned");

        assert!(
            first.with_overflow("m", 350_000).is_none(),
            "a larger refusal says nothing new"
        );

        let tighter = first.with_overflow("m", 200_000).expect("tightened");
        assert!(tighter.ceiling_for("m").expect("learned") < learned);
    }

    /// A refusal proves the ceiling sits below the size refused. It proves
    /// nothing about a configured window being too small, so it must never
    /// raise one — with the shipped default of 128,000, a refusal at 257,963
    /// would otherwise learn 232,166 and start sending far more than the
    /// operator asked for.
    #[test]
    fn a_refusal_cannot_raise_the_configured_ceiling() {
        let ceilings = ContextCeilings {
            default: Some(128_000),
            ..Default::default()
        };

        assert!(
            ceilings
                .with_overflow("openai-chatgpt/gpt-5.6-sol", 257_963)
                .is_none(),
            "a refusal above the configured ceiling says nothing new"
        );

        // One below it still tightens, and stays tightened when the default is
        // later raised.
        let learned = ceilings.with_overflow("m", 100_000).expect("tightened");
        assert_eq!(learned.ceiling_for("m"), Some(90_000));

        let raised = ContextCeilings {
            default: Some(1_050_000),
            learned: learned.learned.clone(),
        };
        assert_eq!(raised.ceiling_for("m"), Some(90_000));
        assert_eq!(raised.ceiling_for("untouched"), Some(1_050_000));
    }

    #[test]
    fn a_nonsense_refusal_is_ignored() {
        let ceilings = ContextCeilings {
            default: Some(128_000),
            ..Default::default()
        };
        assert!(ceilings.with_overflow("m", 0).is_none());
    }
}

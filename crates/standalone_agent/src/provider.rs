//! Local-only provider profiles for the standalone backend.
//!
//! A profile is a user-authored record: endpoint metadata (URL, wire protocol,
//! model id, limits, capability flags) lives next to — but never contains — a
//! credential reference. Secrets are resolved at session-open time and only
//! ever exist in memory as a [`SecretString`](crate::secrets::SecretString).
//!
//! V1 supports exactly one wire protocol: OpenAI-style Chat Completions with
//! streamed text and tool calls. Other protocols are rejected instead of being
//! treated as synonyms.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::protocol::HelperProviderConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireProtocol {
    OpenAiChatCompletions,
}

impl WireProtocol {
    pub const fn helper_api(self) -> &'static str {
        match self {
            WireProtocol::OpenAiChatCompletions => "openai-completions",
        }
    }

    pub fn parse(value: &str) -> Result<Self, ProfileError> {
        match value {
            "openai_chat_completions" | "openai-chat-completions" | "openai-completions" => {
                Ok(WireProtocol::OpenAiChatCompletions)
            }
            other => Err(ProfileError::UnsupportedProtocol(other.to_string())),
        }
    }
}

/// Explicit per-provider compatibility flags. Each flag is opt-in; the default
/// is the most conservative behaviour and never assumes a capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionsCompat {
    /// Provider accepts `role: "developer"` instead of `role: "system"`.
    #[serde(default)]
    pub supports_developer_role: bool,
    /// Provider accepts a `reasoning_effort` parameter.
    #[serde(default)]
    pub supports_reasoning_effort: bool,
    /// Provider emits usage in the final streamed chunk.
    #[serde(default)]
    pub supports_usage_in_streaming: bool,
    /// Provider supports tool_choice (only meaningful when true).
    #[serde(default)]
    pub tool_choice: bool,
    /// Token-limit parameter name: `max_tokens` (default) or `max_completion_tokens`.
    #[serde(default)]
    pub max_completion_tokens_field: bool,
}

impl Default for ChatCompletionsCompat {
    /// Every capability is off unless the user opts in. In particular the
    /// adapter never assumes that a provider reports usage in the stream.
    fn default() -> Self {
        Self {
            supports_developer_role: false,
            supports_reasoning_effort: false,
            supports_usage_in_streaming: false,
            tool_choice: false,
            max_completion_tokens_field: false,
        }
    }
}

impl ChatCompletionsCompat {
    pub fn to_helper_json(&self) -> serde_json::Value {
        serde_json::json!({
            "supportsDeveloperRole": self.supports_developer_role,
            "supportsReasoningEffort": self.supports_reasoning_effort,
            "supportsUsageInStreaming": self.supports_usage_in_streaming,
            "toolChoice": self.tool_choice,
            "maxTokensField": if self.max_completion_tokens_field { "max_completion_tokens" } else { "max_tokens" },
        })
    }
}

/// Where the credential for a profile comes from. The reference is a stable
/// key into the OS secret store; it is never the secret itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialRef {
    /// No authentication: the wire request must not carry an Authorization header.
    None,
    /// Look up by key in the platform secret store.
    SecretStore { key: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProfile {
    /// Stable local identifier chosen by the user. Never sent as a model id.
    pub id: String,
    pub display_name: String,
    /// Base URL of the Chat Completions endpoint.
    pub base_url: String,
    pub wire: WireProtocol,
    /// The provider's own model identifier, sent verbatim as `model`.
    pub model_id: String,
    pub credential: CredentialRef,
    pub context_limit: u64,
    pub output_limit: u64,
    #[serde(default)]
    pub compat: ChatCompletionsCompat,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub supports_image_input: bool,
    /// Extra non-secret headers (never Authorization; use the credential ref).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    #[error("profile id must be a non-empty stable identifier")]
    MissingId,
    #[error("profile display name is required")]
    MissingDisplayName,
    #[error("unsupported wire protocol: {0}")]
    UnsupportedProtocol(String),
    #[error("model id must be a non-empty provider model string (not a local config key)")]
    MissingModelId,
    #[error("model id {0:?} looks like a local config key or UUID; enter the provider's exact model string")]
    ModelIdLooksLikeConfigKey(String),
    #[error("base URL is invalid: {0}")]
    InvalidUrl(String),
    #[error("base URL must use http or https")]
    UnsupportedScheme,
    #[error("base URL must not contain credentials, a query string, or a fragment")]
    UnsafeUrlParts,
    #[error("context limit and output limit must be greater than zero")]
    InvalidLimits,
    #[error("output limit must be smaller than the context limit")]
    OutputLimitTooLarge,
    #[error("header {0:?} must not be set in the profile; use the credential reference")]
    ForbiddenHeader(String),
    #[error("credential reference key must not be empty")]
    MissingCredentialKey,
}

/// Is this value shaped like an internal configuration key rather than a model
/// string? Warp config keys are UUIDs.
pub fn looks_like_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    let mut hyphens = 0;
    for (index, byte) in bytes.iter().enumerate() {
        match (index, byte) {
            (8 | 13 | 18 | 23, b'-') => hyphens += 1,
            (_, b'-') => return false,
            (_, byte) if byte.is_ascii_hexdigit() => {}
            _ => return false,
        }
    }
    hyphens == 4
}

impl ProviderProfile {
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.id.trim().is_empty() {
            return Err(ProfileError::MissingId);
        }
        if self.display_name.trim().is_empty() {
            return Err(ProfileError::MissingDisplayName);
        }
        if self.model_id.trim().is_empty() {
            return Err(ProfileError::MissingModelId);
        }
        if looks_like_uuid(self.model_id.trim()) {
            return Err(ProfileError::ModelIdLooksLikeConfigKey(self.model_id.clone()));
        }
        if self.context_limit == 0 || self.output_limit == 0 {
            return Err(ProfileError::InvalidLimits);
        }
        if self.output_limit >= self.context_limit {
            return Err(ProfileError::OutputLimitTooLarge);
        }
        if let CredentialRef::SecretStore { key } = &self.credential
            && key.trim().is_empty()
        {
            return Err(ProfileError::MissingCredentialKey);
        }
        for header in self.headers.keys() {
            let lower = header.to_ascii_lowercase();
            if lower == "authorization" || lower == "api-key" || lower == "proxy-authorization" {
                return Err(ProfileError::ForbiddenHeader(header.clone()));
            }
        }
        self.normalized_base_url().map(|_| ())
    }

    /// Normalize the user-entered base URL into the form the OpenAI-compatible
    /// client expects: it appends `/chat/completions` itself, so a full
    /// endpoint URL must be reduced to its base.
    ///
    /// Accepted:
    /// - `https://host`
    /// - `https://host/v1`
    /// - `https://host/v1/` (trailing slash)
    /// - `https://host/v1/chat/completions` (full endpoint; suffix stripped)
    ///
    /// Rejected: non-http(s) schemes, credentials in the URL, query/fragment.
    pub fn normalized_base_url(&self) -> Result<String, ProfileError> {
        let trimmed = self.base_url.trim();
        let url = url::Url::parse(trimmed).map_err(|e| ProfileError::InvalidUrl(e.to_string()))?;
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(ProfileError::UnsupportedScheme);
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(ProfileError::UnsafeUrlParts);
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(ProfileError::UnsafeUrlParts);
        }
        let mut path = url.path().trim_end_matches('/').to_string();
        for suffix in ["/chat/completions", "/completions"] {
            if let Some(stripped) = path.strip_suffix(suffix) {
                path = stripped.trim_end_matches('/').to_string();
                break;
            }
        }
        let mut normalized = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
        if let Some(port) = url.port() {
            normalized.push_str(&format!(":{port}"));
        }
        normalized.push_str(&path);
        Ok(normalized)
    }

    /// Whether the endpoint is a loopback address.
    pub fn is_loopback(&self) -> bool {
        url::Url::parse(self.base_url.trim())
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
            .is_some_and(|host| host == "localhost" || host == "127.0.0.1" || host == "::1" || host == "[::1]")
    }

    /// Build the helper's provider description with a resolved credential.
    pub fn helper_config(&self, api_key: Option<&str>) -> Result<HelperProviderConfig, ProfileError> {
        self.validate()?;
        let base_url = self.normalized_base_url()?;
        let auth = match (&self.credential, api_key) {
            (CredentialRef::None, _) => crate::protocol::HelperProviderAuth::None,
            (CredentialRef::SecretStore { .. }, Some(key)) => {
                crate::protocol::HelperProviderAuth::ApiKey { api_key: key.to_string() }
            }
            (CredentialRef::SecretStore { .. }, None) => {
                return Err(ProfileError::MissingCredentialKey);
            }
        };
        let headers = if self.headers.is_empty() {
            None
        } else {
            let mut map = serde_json::Map::new();
            for (key, value) in &self.headers {
                map.insert(key.clone(), serde_json::Value::String(value.clone()));
            }
            Some(map)
        };
        Ok(HelperProviderConfig {
            provider_id: format!("warposs-{}", self.id),
            name: self.display_name.clone(),
            base_url,
            api: self.wire.helper_api().to_string(),
            auth,
            headers,
            model_id: self.model_id.clone(),
            model_name: self.model_id.clone(),
            context_window: self.context_limit,
            max_output_tokens: self.output_limit,
            reasoning: self.reasoning,
            supports_image_input: self.supports_image_input,
            compat: Some(self.compat.to_helper_json()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(base_url: &str, model_id: &str) -> ProviderProfile {
        ProviderProfile {
            id: "p1".into(),
            display_name: "Local".into(),
            base_url: base_url.into(),
            wire: WireProtocol::OpenAiChatCompletions,
            model_id: model_id.into(),
            credential: CredentialRef::None,
            context_limit: 32768,
            output_limit: 4096,
            compat: ChatCompletionsCompat::default(),
            reasoning: false,
            supports_image_input: false,
            headers: BTreeMap::new(),
        }
    }

    #[test]
    fn normalizes_base_urls_without_duplicating_v1_or_the_endpoint() {
        let cases = [
            ("http://127.0.0.1:8080", "http://127.0.0.1:8080"),
            ("http://127.0.0.1:8080/", "http://127.0.0.1:8080"),
            ("http://127.0.0.1:8080/v1", "http://127.0.0.1:8080/v1"),
            ("http://127.0.0.1:8080/v1/", "http://127.0.0.1:8080/v1"),
            ("http://127.0.0.1:8080/v1/chat/completions", "http://127.0.0.1:8080/v1"),
            ("https://api.example.com/v1/chat/completions", "https://api.example.com/v1"),
        ];
        for (input, expected) in cases {
            let actual = profile(input, "gpt-x").normalized_base_url().expect("valid");
            assert_eq!(actual, expected, "input {input}");
            // The client appends /chat/completions exactly once.
            assert_eq!(format!("{actual}/chat/completions"), format!("{expected}/chat/completions"));
            assert!(!actual.ends_with("/chat/completions"));
        }
    }

    #[test]
    fn rejects_credentials_in_urls_queries_and_unsupported_protocols() {
        assert_eq!(
            profile("ftp://example.com", "m").normalized_base_url(),
            Err(ProfileError::UnsupportedScheme)
        );
        assert_eq!(
            profile("https://user:pass@example.com/v1", "m").normalized_base_url(),
            Err(ProfileError::UnsafeUrlParts)
        );
        assert_eq!(
            profile("https://example.com/v1?key=secret", "m").normalized_base_url(),
            Err(ProfileError::UnsafeUrlParts)
        );
        assert!(matches!(
            WireProtocol::parse("anthropic_messages"),
            Err(ProfileError::UnsupportedProtocol(_))
        ));
        assert!(matches!(
            WireProtocol::parse("openai_responses"),
            Err(ProfileError::UnsupportedProtocol(_))
        ));
    }

    #[test]
    fn refuses_config_keys_as_model_ids_and_limits_inversions() {
        let mut bad = profile("http://127.0.0.1:1", "0b6a1b1c-2b0d-4f39-9a3f-2f1c0c8f1a11");
        assert!(matches!(
            bad.validate(),
            Err(ProfileError::ModelIdLooksLikeConfigKey(_))
        ));
        bad.model_id = "real-model".into();
        bad.output_limit = bad.context_limit;
        assert_eq!(bad.validate(), Err(ProfileError::OutputLimitTooLarge));
        bad.output_limit = 1024;
        bad.credential = CredentialRef::SecretStore { key: "  ".into() };
        assert_eq!(bad.validate(), Err(ProfileError::MissingCredentialKey));
    }

    #[test]
    fn partial_compat_objects_deserialize_with_conservative_defaults() {
        // Hand-written config files routinely omit compat fields; the defaults
        // must be the conservative ones (no developer role, no reasoning).
        let profile: ProviderProfile = serde_json::from_str(
            r#"{
                "id": "p",
                "display_name": "P",
                "base_url": "http://127.0.0.1:8080/v1",
                "wire": "open_ai_chat_completions",
                "model_id": "m",
                "credential": "none",
                "context_limit": 8192,
                "output_limit": 1024,
                "compat": {}
            }"#,
        )
        .expect("partial profile deserializes");
        assert!(!profile.compat.supports_developer_role);
        assert!(!profile.compat.supports_reasoning_effort);
        assert!(!profile.compat.supports_usage_in_streaming);
        assert!(!profile.compat.tool_choice);

        let minimal: ProviderProfile = serde_json::from_str(
            r#"{
                "id": "p",
                "display_name": "P",
                "base_url": "http://127.0.0.1:8080/v1",
                "wire": "open_ai_chat_completions",
                "model_id": "m",
                "credential": "none",
                "context_limit": 8192,
                "output_limit": 1024
            }"#,
        )
        .expect("profile without compat deserializes");
        assert!(!minimal.compat.supports_developer_role);
    }

    #[test]
    fn auth_none_resolves_without_a_key_and_never_carries_one() {
        let config = profile("http://127.0.0.1:8080/v1", "m")
            .helper_config(None)
            .expect("auth none needs no key");
        assert!(matches!(config.auth, crate::protocol::HelperProviderAuth::None));
        let serialized = serde_json::to_string(&config).expect("serializes");
        assert!(!serialized.to_ascii_lowercase().contains("authorization"));
    }

    #[test]
    fn api_key_profiles_require_a_resolved_secret() {
        let mut with_key = profile("https://api.example.com/v1", "m");
        with_key.credential = CredentialRef::SecretStore { key: "warposs/p1".into() };
        assert!(with_key.helper_config(None).is_err());
        let config = with_key.helper_config(Some("sk-secret")).expect("resolves");
        let serialized = serde_json::to_string(&config).expect("serializes");
        assert!(serialized.contains("sk-secret"));
        assert!(!format!("{config:?}").contains("sk-secret"), "Debug must not leak the key");
    }
}

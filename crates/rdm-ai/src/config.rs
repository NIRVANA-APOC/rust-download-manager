//! AI provider configuration (`docs/design-next-features.md` §3.2).
//!
//! Persisted as `<state_dir>/ai.json`. The API key lives **only** in that file
//! (or the `RDM_AI_API_KEY` environment variable); it is never logged and is
//! redacted from `Debug`.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::write_atomic;

/// Environment variable that overrides the stored API key (testing / CI).
pub const ENV_API_KEY: &str = "RDM_AI_API_KEY";
/// Default OpenAI-compatible base URL (DeepSeek).
pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com/v1";
/// Default model.
pub const DEFAULT_MODEL: &str = "deepseek-flash";
/// Default request timeout in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Chat-completions path appended to `base_url`.
pub const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";

/// OpenAI-compatible provider configuration.
///
/// `Debug` is hand-written so the API key can never end up in logs: the field
/// is shown as `<redacted>` (or `<empty>`). Wire format is unaffected.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AiConfig {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub timeout_secs: u64,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: DEFAULT_BASE_URL.to_owned(),
            model: DEFAULT_MODEL.to_owned(),
            api_key: String::new(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }
}

impl std::fmt::Debug for AiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiConfig")
            .field("enabled", &self.enabled)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .field("timeout_secs", &self.timeout_secs)
            .finish()
    }
}

impl AiConfig {
    /// Chat-completions endpoint. Tolerates a trailing `/`, an empty base
    /// (falls back to the default provider) and a base that already ends in
    /// `/chat/completions`.
    pub fn endpoint(&self) -> String {
        let base = self.base_url.trim();
        let base = if base.is_empty() {
            DEFAULT_BASE_URL
        } else {
            base
        };
        let base = base.trim_end_matches('/');
        if base.to_ascii_lowercase().ends_with(CHAT_COMPLETIONS_PATH) {
            base.to_owned()
        } else {
            format!("{base}{CHAT_COMPLETIONS_PATH}")
        }
    }

    /// The API key actually used: `RDM_AI_API_KEY` wins when non-empty.
    pub fn resolved_api_key(&self) -> String {
        let from_env = std::env::var(ENV_API_KEY).unwrap_or_default();
        let from_env = from_env.trim();
        if from_env.is_empty() {
            self.api_key.trim().to_owned()
        } else {
            from_env.to_owned()
        }
    }

    /// AI features require both the switch and a key.
    pub fn is_configured(&self) -> bool {
        self.enabled && !self.resolved_api_key().is_empty()
    }

    /// `<state_dir>/ai.json`.
    pub fn config_path(state_dir: &Path) -> PathBuf {
        state_dir.join("ai.json")
    }

    /// Load the config; a missing or corrupted file yields the defaults.
    pub fn load(state_dir: &Path) -> Self {
        let path = Self::config_path(state_dir);
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Persist the config with an atomic write (owner-only on Unix).
    pub fn save(&self, state_dir: &Path) -> io::Result<()> {
        let path = Self::config_path(state_dir);
        let text = serde_json::to_string_pretty(self)
            .map_err(|error| io::Error::other(format!("cannot serialize ai.json: {error}")))?;
        write_atomic(&path, &text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ENV_LOCK;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "rdm-ai-config-{}-{tag}-{}",
                std::process::id(),
                crate::now_ms()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn without_env_key<T>(f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var(ENV_API_KEY).ok();
        std::env::remove_var(ENV_API_KEY);
        let result = f();
        if let Some(previous) = previous {
            std::env::set_var(ENV_API_KEY, previous);
        }
        result
    }

    #[test]
    fn defaults_match_contract() {
        let config = AiConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.base_url, "https://api.deepseek.com/v1");
        assert_eq!(config.model, "deepseek-flash");
        assert_eq!(config.api_key, "");
        assert_eq!(config.timeout_secs, 30);
        assert!(!without_env_key(|| config.is_configured()));
    }

    #[test]
    fn serde_defaults_fill_missing_fields() {
        let config: AiConfig = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert!(config.enabled);
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.model, DEFAULT_MODEL);
        assert_eq!(config.timeout_secs, DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn endpoint_joins_base_url() {
        let mut config = AiConfig::default();
        assert_eq!(
            config.endpoint(),
            "https://api.deepseek.com/v1/chat/completions"
        );
        config.base_url = "http://127.0.0.1:1234/v1/".into();
        assert_eq!(
            config.endpoint(),
            "http://127.0.0.1:1234/v1/chat/completions"
        );
        config.base_url = "http://127.0.0.1:1234/v1/chat/completions".into();
        assert_eq!(
            config.endpoint(),
            "http://127.0.0.1:1234/v1/chat/completions"
        );
        config.base_url = "http://127.0.0.1:1234/v1/chat/completions/".into();
        assert_eq!(
            config.endpoint(),
            "http://127.0.0.1:1234/v1/chat/completions"
        );
        config.base_url = String::new();
        assert_eq!(
            config.endpoint(),
            DEFAULT_BASE_URL.to_owned() + CHAT_COMPLETIONS_PATH
        );
    }

    #[test]
    fn is_configured_requires_switch_and_key() {
        without_env_key(|| {
            let mut config = AiConfig::default();
            assert!(!config.is_configured());
            config.enabled = true;
            assert!(!config.is_configured());
            config.api_key = "secret-key".into();
            assert!(config.is_configured());
            config.enabled = false;
            assert!(!config.is_configured());
        });
    }

    #[test]
    fn env_key_wins_over_stored_key() {
        without_env_key(|| {
            let mut config = AiConfig {
                enabled: true,
                api_key: "stored".into(),
                ..AiConfig::default()
            };
            assert_eq!(config.resolved_api_key(), "stored");
            std::env::set_var(ENV_API_KEY, "  from-env  ");
            assert_eq!(config.resolved_api_key(), "from-env");
            assert!(config.is_configured());
            std::env::set_var(ENV_API_KEY, "");
            assert_eq!(config.resolved_api_key(), "stored");
            std::env::remove_var(ENV_API_KEY);
            config.api_key = String::new();
            assert!(!config.is_configured());
        });
    }

    #[test]
    fn debug_redacts_api_key() {
        let config = AiConfig {
            enabled: true,
            api_key: "super-secret-key".into(),
            ..AiConfig::default()
        };
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("super-secret-key"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn save_and_load_roundtrip() {
        let state = TempDir::new("roundtrip");
        assert_eq!(AiConfig::load(&state.0), AiConfig::default());

        let config = AiConfig {
            enabled: true,
            base_url: "http://localhost:8080/v1".into(),
            model: "test-model".into(),
            api_key: "key-123".into(),
            timeout_secs: 7,
        };
        config.save(&state.0).unwrap();
        assert_eq!(AiConfig::config_path(&state.0), state.0.join("ai.json"));
        assert_eq!(AiConfig::load(&state.0), config);

        // Corrupted files fall back to defaults instead of erroring.
        std::fs::write(AiConfig::config_path(&state.0), "{not json").unwrap();
        assert_eq!(AiConfig::load(&state.0), AiConfig::default());
    }
}

//! AI provider configuration (`docs/design-next-features.md` §3.2,
//! `docs/design/16-reuse-and-simplification.md` §5).
//!
//! Persisted as `<state_dir>/ai.json`. The API key lives **only** in that file
//! (or the `RDM_AI_API_KEY` environment variable); it is never logged and is
//! redacted from `Debug` — including every per-provider profile's key.
//!
//! ## Per-provider profiles (`docs/design/16` §5.1)
//!
//! [`AiConfig`] keeps the original flat fields
//! (`enabled`/`base_url`/`model`/`api_key`/`timeout_secs`/`provider`) as the
//! **currently active working values**, so every existing consumer (resolve,
//! parse, chat, monitor) keeps working unchanged. It additionally stores one
//! independent [`AiProfile`] per provider:
//!
//! * `active_profile` — id of the profile in effect;
//! * `profiles` — the profiles themselves (`deepseek`/`openai`/`siliconflow`
//!   builtins plus any number of `custom-*` profiles).
//!
//! [`AiConfig::activate_profile`] first writes the active flat values back into
//! the outgoing profile, then loads the target profile's
//! `base_url`/`api_key`/`model` into the flat fields. This is what fixes the
//! original bug where switching providers kept the previous provider's API key.
//!
//! Old `ai.json` files (no `profiles`) are migrated automatically by
//! [`AiConfig::load`]: the flat fields are folded into the profile of the
//! provider then in effect and the migrated layout is written back once.
//!
//! `provider` is the chosen LLM preset (`docs/design/14-llm-provider-pricing.md`).
//! It is optional on the wire: configs written before the preset existed simply
//! omit it, and [`AiConfig::resolved_provider`] then infers the family from the
//! base URL (unknown endpoints become [`ProviderKind::Custom`]).

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::providers::{
    self, CostEstimate, ModelPrice, ProviderKind, ProviderPreset, PRICE_CATALOG_DATE, USD_TO_CNY,
};
use crate::usage::TokenUsage;
use crate::{now_ms, write_atomic};

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
/// Model-listing path appended to `base_url` (`GET /models`).
pub const MODELS_PATH: &str = "/models";

/// One provider profile: its own endpoint, API key and selected model.
///
/// Builtin profiles (`id = "deepseek" | "openai" | "siliconflow"`) are seeded
/// from [`providers::preset`] and cannot be deleted. Custom profiles are
/// user-created OpenAI-compatible gateways (`id = "custom-<n>"`).
///
/// `Debug` is hand-written so a profile's API key can never end up in logs.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiProfile {
    /// Stable profile id (`deepseek`/`openai`/`siliconflow`/`custom-1`…).
    #[serde(default)]
    pub id: String,
    /// Human-readable name shown in the settings page.
    #[serde(default)]
    pub name: String,
    /// Provider family used for the bundled price catalog. `None` for custom
    /// profiles (no prices); present for builtin profiles.
    #[serde(default)]
    pub kind: Option<ProviderKind>,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    /// Selected model id of this profile.
    #[serde(default)]
    pub model: String,
    /// Last known model list for this profile; seeded from the bundled catalog
    /// for builtins and refreshed by `GET {base_url}/models`.
    #[serde(default)]
    pub models: Vec<String>,
    /// Last time this profile was edited / refreshed (Unix epoch ms; 0 = never).
    #[serde(default)]
    pub updated_ms: u64,
}

impl AiProfile {
    /// Builtin profile seeded from the bundled preset (empty key, catalog
    /// models, default endpoint/model).
    pub fn builtin(kind: ProviderKind) -> Self {
        let preset = providers::preset(kind);
        Self {
            id: kind.id().to_owned(),
            name: kind.label_zh().to_owned(),
            kind: Some(kind),
            base_url: preset
                .map(|preset| preset.base_url.to_owned())
                .unwrap_or_else(|| kind.default_base_url().to_owned()),
            api_key: String::new(),
            model: kind.default_model().to_owned(),
            models: kind.catalog_model_ids(),
            updated_ms: 0,
        }
    }

    /// Empty custom profile (user edits endpoint/key/model afterwards).
    pub fn custom(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            kind: Some(ProviderKind::Custom),
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            models: Vec::new(),
            updated_ms: 0,
        }
    }

    /// `true` for the bundled `deepseek` / `openai` / `siliconflow` profiles.
    pub fn is_builtin(&self) -> bool {
        is_builtin_profile_id(&self.id)
    }

    /// Provider family used for price estimation / UI defaults.
    pub fn effective_kind(&self) -> ProviderKind {
        if let Some(kind) = self.kind {
            return kind;
        }
        ProviderKind::infer_from_base_url(&self.base_url).unwrap_or(ProviderKind::Custom)
    }

    /// Bundled preset of the profile family (`None` for custom profiles).
    pub fn preset(&self) -> Option<&'static ProviderPreset> {
        providers::preset(self.effective_kind())
    }

    /// Name for the UI; falls back to the id when unnamed.
    pub fn display_name(&self) -> &str {
        let name = self.name.trim();
        if name.is_empty() {
            self.id.as_str()
        } else {
            name
        }
    }

    /// Replace the cached model list with a trimmed, case-insensitively deduped
    /// and sorted one (empty ids are dropped).
    pub fn set_models(&mut self, models: impl IntoIterator<Item = String>) {
        let mut unique: BTreeMap<String, String> = BTreeMap::new();
        for model in models {
            let model = model.trim();
            if model.is_empty() {
                continue;
            }
            unique
                .entry(model.to_ascii_lowercase())
                .or_insert_with(|| model.to_owned());
        }
        self.models = unique.into_values().collect();
        self.updated_ms = now_ms();
    }

    /// Case-insensitive model-id comparison (provider ids are not consistently
    /// cased across endpoints).
    pub fn matches_model(&self, model: &str) -> bool {
        self.model.trim().eq_ignore_ascii_case(model.trim())
    }
}

impl std::fmt::Debug for AiProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiProfile")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("models", &self.models.len())
            .field("updated_ms", &self.updated_ms)
            .field(
                "api_key",
                &if self.api_key.trim().is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .finish()
    }
}

/// The ids of the builtin provider profiles, in display order.
pub fn builtin_profile_ids() -> [&'static str; 3] {
    [
        ProviderKind::DeepSeek.id(),
        ProviderKind::OpenAI.id(),
        ProviderKind::SiliconFlow.id(),
    ]
}

/// `true` for `deepseek` / `openai` / `siliconflow` (builtin profiles cannot be
/// deleted).
pub fn is_builtin_profile_id(id: &str) -> bool {
    let id = id.trim();
    builtin_profile_ids()
        .iter()
        .any(|builtin| builtin.eq_ignore_ascii_case(id))
}

fn builtin_profiles() -> BTreeMap<String, AiProfile> {
    ProviderKind::BUILTIN
        .iter()
        .copied()
        .map(|kind| {
            let profile = AiProfile::builtin(kind);
            (profile.id.clone(), profile)
        })
        .collect()
}

/// OpenAI-compatible provider configuration.
///
/// The flat fields are the currently active working values (unchanged for every
/// existing consumer); `active_profile`/`profiles` persist one independent
/// profile per provider. See the module docs for the activation/migration rules.
///
/// `Debug` is hand-written so the API keys can never end up in logs: they are
/// shown as `<redacted>` (or `<empty>`), including the keys inside `profiles`.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Selected LLM preset; `None` for legacy configs (inferred from `base_url`).
    #[serde(default)]
    pub provider: Option<ProviderKind>,
    /// Id of the profile currently loaded into the flat fields; empty for
    /// legacy configs before [`AiConfig::load`] has migrated them.
    #[serde(default)]
    pub active_profile: String,
    /// One independent profile per provider (builtin + user-defined).
    #[serde(default)]
    pub profiles: BTreeMap<String, AiProfile>,
}

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_owned()
}

fn default_model() -> String {
    DEFAULT_MODEL.to_owned()
}

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: DEFAULT_BASE_URL.to_owned(),
            model: DEFAULT_MODEL.to_owned(),
            api_key: String::new(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            provider: Some(ProviderKind::DeepSeek),
            active_profile: ProviderKind::DeepSeek.id().to_owned(),
            profiles: builtin_profiles(),
        }
    }
}

impl std::fmt::Debug for AiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiConfig")
            .field("enabled", &self.enabled)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("provider", &self.provider)
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .field("timeout_secs", &self.timeout_secs)
            .field("active_profile", &self.active_profile)
            .field("profiles", &self.profiles)
            .finish()
    }
}

impl AiConfig {
    /// Chat-completions endpoint. Tolerates a trailing `/`, an empty base
    /// (falls back to the default provider), a base that already ends in
    /// `/chat/completions` and a base that ends in `/models`.
    pub fn endpoint(&self) -> String {
        api_endpoint(&self.base_url, CHAT_COMPLETIONS_PATH)
    }

    /// Model-listing endpoint (`GET`, `Authorization: Bearer <key>`).
    ///
    /// Same tolerance as [`endpoint`](Self::endpoint): empty base → default
    /// provider, trailing `/` ignored, `/chat/completions` and `/models`
    /// suffixes normalized.
    pub fn models_endpoint(&self) -> String {
        api_endpoint(&self.base_url, MODELS_PATH)
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

    /// The provider preset in effect: the explicit `provider` when set, else the
    /// family inferred from `base_url`, else [`ProviderKind::Custom`].
    pub fn resolved_provider(&self) -> ProviderKind {
        if let Some(provider) = self.provider {
            return provider;
        }
        ProviderKind::infer_from_base_url(&self.base_url).unwrap_or(ProviderKind::Custom)
    }

    /// Switch to a provider preset.
    ///
    /// Bundled providers get their default base URL and first catalog model.
    /// [`ProviderKind::Custom`] never forces an endpoint: the current
    /// `base_url`/`model` are kept so the user can point at a proxy and type
    /// their own model id.
    ///
    /// This only changes the flat working values; use
    /// [`activate_profile`](Self::activate_profile) for the profile-aware
    /// provider switch that also swaps the API key.
    pub fn apply_provider(&mut self, kind: ProviderKind) {
        self.provider = Some(kind);
        let Some(preset) = providers::preset(kind) else {
            // Custom: keep the user's endpoint/model for editing.
            return;
        };
        self.base_url = preset.base_url.to_owned();
        if let Some(first) = preset.models.first() {
            self.model = first.model.to_owned();
        }
    }

    /// Preset of the resolved provider (`None` for a custom endpoint).
    pub fn provider_preset(&self) -> Option<&'static ProviderPreset> {
        providers::preset(self.resolved_provider())
    }

    /// Price catalog entry of the configured model, when the provider has one.
    pub fn model_price(&self) -> Option<&'static ModelPrice> {
        providers::find_model(self.resolved_provider(), &self.model)
    }

    /// Estimated cost of one token usage under this config at `at_ms`.
    pub fn cost_estimate(&self, usage: &TokenUsage, at_ms: u64) -> CostEstimate {
        providers::estimate_cost(self.resolved_provider(), &self.model, usage, at_ms)
    }

    /// Short Chinese footnote for the cost figures, e.g.
    /// `"≈ 估算 · 价目快照 2026-10-22 · USD→CNY 7.2"`.
    ///
    /// `None` when the model has no price catalog (custom endpoint / unknown
    /// model): the UI then shows "no price estimate" instead of a fake `¥0`.
    pub fn price_note_zh(&self) -> Option<String> {
        let price = self.model_price()?;
        let mut note = format!("≈ 估算 · 价目快照 {PRICE_CATALOG_DATE}");
        if price.currency == crate::providers::Currency::Usd {
            note.push_str(&format!(" · USD→CNY {USD_TO_CNY}"));
        }
        Some(note)
    }

    // -----------------------------------------------------------------------
    // Profiles
    // -----------------------------------------------------------------------

    /// Every profile in display order: the builtins (`deepseek`, `openai`,
    /// `siliconflow`) first, then custom profiles sorted by name.
    pub fn profiles(&self) -> Vec<&AiProfile> {
        let mut ordered: Vec<&AiProfile> = Vec::with_capacity(self.profiles.len());
        for kind in ProviderKind::BUILTIN {
            if let Some(profile) = self.profiles.get(kind.id()) {
                ordered.push(profile);
            }
        }
        let mut custom: Vec<&AiProfile> = self
            .profiles
            .values()
            .filter(|profile| !profile.is_builtin())
            .collect();
        custom.sort_by(|left, right| {
            (left.display_name().to_lowercase(), left.id.to_lowercase())
                .cmp(&(right.display_name().to_lowercase(), right.id.to_lowercase()))
        });
        ordered.extend(custom);
        ordered
    }

    /// Look up one profile by id.
    pub fn profile(&self, id: &str) -> Option<&AiProfile> {
        self.profiles.get(id.trim())
    }

    /// Mutable lookup of one profile by id.
    pub fn profile_mut(&mut self, id: &str) -> Option<&mut AiProfile> {
        self.profiles.get_mut(id.trim())
    }

    /// Id of the active profile; falls back to the provider in effect when the
    /// stored id is missing (legacy config read before [`migrate`](Self::migrate)).
    pub fn active_profile_id(&self) -> &str {
        if !self.active_profile.is_empty() {
            return self.active_profile.as_str();
        }
        self.resolved_provider().id()
    }

    /// The active profile, when it exists.
    pub fn active_profile(&self) -> Option<&AiProfile> {
        self.profiles.get(self.active_profile_id())
    }

    /// Cached model ids of the active profile (`[]` when there is none).
    pub fn active_models(&self) -> Vec<String> {
        self.active_profile()
            .map(|profile| profile.models.clone())
            .unwrap_or_default()
    }

    /// Insert the builtin profiles that are missing; `true` when anything was
    /// added.
    pub fn ensure_builtin_profiles(&mut self) -> bool {
        let mut changed = false;
        for kind in ProviderKind::BUILTIN {
            if !self.profiles.contains_key(kind.id()) {
                let profile = AiProfile::builtin(kind);
                self.profiles.insert(kind.id().to_owned(), profile);
                changed = true;
            }
        }
        changed
    }

    /// Fill in missing builtin profiles, fold a legacy flat config (no
    /// `profiles`) into a profile and repair an empty/dangling
    /// `active_profile`. Returns `true` when the config changed.
    ///
    /// Idempotent: running it on an already-migrated config changes nothing.
    pub fn migrate(&mut self) -> bool {
        let had_profiles = !self.profiles.is_empty();
        let mut changed = self.ensure_builtin_profiles();

        if !had_profiles {
            // Legacy `ai.json` (written before profiles existed): the flat
            // fields are the only state — fold them into one profile for the
            // provider currently in effect.
            let kind = self.resolved_provider();
            let id = if kind == ProviderKind::Custom {
                self.unique_custom_id()
            } else {
                kind.id().to_owned()
            };
            let models = if kind == ProviderKind::Custom {
                Vec::new()
            } else {
                self.profiles
                    .get(&id)
                    .map(|profile| profile.models.clone())
                    .filter(|models| !models.is_empty())
                    .unwrap_or_else(|| kind.catalog_model_ids())
            };
            let name = if kind == ProviderKind::Custom {
                "自定义 Provider".to_owned()
            } else {
                kind.label_zh().to_owned()
            };
            let profile = AiProfile {
                id: id.clone(),
                name,
                kind: Some(kind),
                base_url: self.base_url.clone(),
                api_key: self.api_key.clone(),
                model: self.model.clone(),
                models,
                updated_ms: now_ms(),
            };
            self.profiles.insert(id.clone(), profile);
            self.active_profile = id;
            self.provider = Some(kind);
            changed = true;
        } else if self.active_profile.is_empty()
            || !self.profiles.contains_key(&self.active_profile)
        {
            let inferred = self.resolved_provider();
            let fallback = if self.profiles.contains_key(inferred.id()) {
                Some(inferred.id().to_owned())
            } else {
                self.profiles().first().map(|profile| profile.id.clone())
            };
            if let Some(fallback) = fallback {
                self.active_profile = fallback;
                changed = true;
            }
        }
        changed
    }

    /// Write the flat (active) values back into the active profile.
    ///
    /// Call this before persisting edits made through the flat fields, and it is
    /// also the first half of [`activate_profile`](Self::activate_profile).
    /// No-op when no profile is active.
    pub fn sync_active_profile(&mut self) {
        if self.active_profile.is_empty() {
            return;
        }
        let id = self.active_profile.clone();
        let provider = self.provider;
        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();
        let model = self.model.clone();
        let Some(profile) = self.profiles.get_mut(&id) else {
            return;
        };
        profile.base_url = base_url;
        profile.api_key = api_key;
        profile.model = model;
        if let Some(kind) = provider {
            profile.kind = Some(kind);
        }
        profile.updated_ms = now_ms();
    }

    /// Switch the active profile.
    ///
    /// Saves the current flat values back into the outgoing profile, then loads
    /// the target profile's `base_url`/`api_key`/`model` into the flat fields so
    /// all three always switch together.
    ///
    /// Returns `false` for an unknown/empty id or when that profile is already
    /// active.
    pub fn activate_profile(&mut self, id: &str) -> bool {
        let id = id.trim();
        if id.is_empty() || !self.profiles.contains_key(id) {
            return false;
        }
        if self.active_profile == id {
            return false;
        }
        if !self.active_profile.is_empty() {
            self.sync_active_profile();
        }
        let Some(target) = self.profiles.get(id).cloned() else {
            return false;
        };
        let provider = target
            .kind
            .or_else(|| ProviderKind::infer_from_base_url(&target.base_url));
        self.active_profile = id.to_owned();
        self.base_url = target.base_url;
        self.model = target.model;
        self.api_key = target.api_key;
        self.provider = provider;
        true
    }

    /// Insert or replace one profile (id/name are trimmed; missing ids/names are
    /// generated). Returns the profile id.
    pub fn upsert_profile(&mut self, mut profile: AiProfile) -> String {
        if profile.id.trim().is_empty() {
            profile.id = self.unique_custom_id();
        }
        profile.id = profile.id.trim().to_owned();
        profile.name = profile.name.trim().to_owned();
        if profile.name.is_empty() {
            profile.name = profile.id.clone();
        }
        profile.updated_ms = now_ms();
        let id = profile.id.clone();
        self.profiles.insert(id.clone(), profile);
        id
    }

    /// Create and store a new empty custom profile; returns its id.
    ///
    /// The caller decides when to [`activate_profile`](Self::activate_profile).
    pub fn add_custom_profile(&mut self) -> String {
        let id = self.unique_custom_id();
        let index = self
            .profiles
            .values()
            .filter(|profile| !profile.is_builtin())
            .count()
            + 1;
        let profile = AiProfile::custom(id, format!("自定义 Provider {index}"));
        self.upsert_profile(profile)
    }

    /// Delete a custom profile. Builtin profiles are protected and unknown ids
    /// return `false`. When the deleted profile was active, the first remaining
    /// profile (a builtin) becomes active.
    pub fn remove_profile(&mut self, id: &str) -> bool {
        let id = id.trim();
        if id.is_empty() || is_builtin_profile_id(id) {
            return false;
        }
        if self.profiles.remove(id).is_none() {
            return false;
        }
        if self.active_profile == id {
            self.active_profile.clear();
            if let Some(fallback) = self.profiles().first().map(|profile| profile.id.clone()) {
                self.activate_profile(&fallback);
            }
        }
        true
    }

    /// Rename the active profile. Empty names and unknown/empty active profiles
    /// are rejected (`false`); returns `true` when the name changed.
    pub fn rename_active_profile(&mut self, name: &str) -> bool {
        let name = name.trim();
        let id = self.active_profile.clone();
        if name.is_empty() || id.is_empty() {
            return false;
        }
        let Some(profile) = self.profiles.get_mut(&id) else {
            return false;
        };
        if profile.name == name {
            return false;
        }
        profile.name = name.to_owned();
        profile.updated_ms = now_ms();
        true
    }

    /// Replace the active profile's cached model list (sorted/deduped).
    pub fn set_active_models(&mut self, models: impl IntoIterator<Item = String>) {
        let id = self.active_profile_id().to_owned();
        if let Some(profile) = self.profiles.get_mut(&id) {
            profile.set_models(models);
        }
    }

    /// First unused `custom-<n>` id.
    pub fn unique_custom_id(&self) -> String {
        for index in 1..=9_999u32 {
            let id = format!("custom-{index}");
            if !self.profiles.contains_key(&id) {
                return id;
            }
        }
        format!("custom-{}", now_ms())
    }

    // -----------------------------------------------------------------------
    // Persistence
    // -----------------------------------------------------------------------

    /// `<state_dir>/ai.json`.
    pub fn config_path(state_dir: &Path) -> PathBuf {
        state_dir.join("ai.json")
    }

    /// Load the config; a missing or corrupted file yields the defaults.
    ///
    /// A legacy file (no `profiles`) is migrated and written back once.
    pub fn load(state_dir: &Path) -> Self {
        let path = Self::config_path(state_dir);
        let mut config: AiConfig = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        if config.migrate() {
            // Best-effort persistence of the migrated layout; the in-memory
            // config is already usable when the write fails (read-only dir …).
            let _ = config.save(state_dir);
        }
        config
    }

    /// Persist the config with an atomic write (owner-only on Unix).
    ///
    /// The written snapshot is migrated first, so `ai.json` always carries the
    /// per-provider profiles even when the in-memory config came from a legacy
    /// code path.
    pub fn save(&self, state_dir: &Path) -> io::Result<()> {
        let mut snapshot = self.clone();
        snapshot.migrate();
        let path = Self::config_path(state_dir);
        let text = serde_json::to_string_pretty(&snapshot)
            .map_err(|error| io::Error::other(format!("cannot serialize ai.json: {error}")))?;
        write_atomic(&path, &text)
    }
}

/// Join `suffix` onto an OpenAI-compatible base URL, tolerating an empty base,
/// trailing slashes and a base that already ends in `/chat/completions` or
/// `/models`.
fn api_endpoint(base_url: &str, suffix: &str) -> String {
    let base = base_url.trim();
    let base = if base.is_empty() {
        DEFAULT_BASE_URL
    } else {
        base
    };
    let base = base.trim_end_matches('/');
    let lower = base.to_ascii_lowercase();
    for known in [CHAT_COMPLETIONS_PATH, MODELS_PATH] {
        if lower.ends_with(known) {
            let root = base[..base.len() - known.len()].trim_end_matches('/');
            return format!("{root}{suffix}");
        }
    }
    format!("{base}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ENV_LOCK;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            // `now_ms()` alone collides when parallel tests start in the same
            // millisecond (two tests then share/remove one directory); the
            // process-wide counter keeps every directory unique.
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rdm-ai-config-{}-{tag}-{}-{sequence}",
                std::process::id(),
                crate::now_ms()
            ));
            std::fs::create_dir_all(&path)
                .unwrap_or_else(|error| panic!("create_dir_all({path:?}) failed: {error}"));
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
        assert_eq!(config.provider, Some(ProviderKind::DeepSeek));
        assert_eq!(config.resolved_provider(), ProviderKind::DeepSeek);
        assert_eq!(config.base_url, ProviderKind::DeepSeek.default_base_url());
        assert_eq!(config.model, ProviderKind::DeepSeek.default_model());
        assert!(!without_env_key(|| config.is_configured()));

        // Profiles: one builtin per bundled provider, deepseek active.
        assert_eq!(config.active_profile, "deepseek");
        assert_eq!(config.profiles.len(), 3);
        let ids: Vec<&str> = config
            .profiles()
            .iter()
            .map(|profile| profile.id.as_str())
            .collect();
        assert_eq!(ids, ["deepseek", "openai", "siliconflow"]);
        let active = config.active_profile().expect("active profile");
        assert_eq!(active.base_url, config.base_url);
        assert_eq!(active.model, config.model);
        assert_eq!(
            active.models,
            ProviderKind::DeepSeek.catalog_model_ids(),
            "builtin profiles are seeded with the bundled catalog"
        );
        assert!(config.profiles().iter().all(|profile| profile.is_builtin()));
    }

    #[test]
    fn serde_defaults_fill_missing_fields() {
        let config: AiConfig = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert!(config.enabled);
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.model, DEFAULT_MODEL);
        assert_eq!(config.timeout_secs, DEFAULT_TIMEOUT_SECS);
        // No explicit preset: resolved from the (default) base URL.
        assert_eq!(config.provider, None);
        assert_eq!(config.resolved_provider(), ProviderKind::DeepSeek);
        // A legacy wire format has no profiles until it is loaded/migrated.
        assert!(config.profiles.is_empty());
        assert!(config.active_profile.is_empty());
    }

    #[test]
    fn legacy_config_without_provider_infers_it_from_base_url() {
        let openai: AiConfig = serde_json::from_str(
            r#"{"enabled":true,"base_url":"https://api.openai.com/v1","model":"gpt-4o-mini","api_key":"k"}"#,
        )
        .unwrap();
        assert_eq!(openai.provider, None);
        assert_eq!(openai.resolved_provider(), ProviderKind::OpenAI);
        assert_eq!(
            openai.model_price().map(|price| price.model),
            Some("gpt-4o-mini")
        );

        let mock: AiConfig =
            serde_json::from_str(r#"{"base_url":"http://127.0.0.1:9999/v1"}"#).unwrap();
        assert_eq!(mock.resolved_provider(), ProviderKind::Custom);
        assert!(mock.provider_preset().is_none());
        assert!(mock.model_price().is_none());
        assert!(mock.price_note_zh().is_none());

        // An explicit preset wins even when the base URL is a custom proxy.
        let explicit: AiConfig = serde_json::from_str(
            r#"{"provider":"deepseek","base_url":"http://127.0.0.1:1234/v1","model":"deepseek-flash"}"#,
        )
        .unwrap();
        assert_eq!(explicit.resolved_provider(), ProviderKind::DeepSeek);
        assert_eq!(
            explicit.model_price().map(|price| price.model),
            Some("deepseek-flash")
        );
    }

    #[test]
    fn legacy_ai_json_is_migrated_into_the_resolved_provider_profile() {
        let state = TempDir::new("migrate-openai");
        std::fs::write(
            AiConfig::config_path(&state.0),
            r#"{"enabled":true,"base_url":"https://api.openai.com/v1","model":"gpt-4o-mini","api_key":"legacy-openai-key","timeout_secs":9}"#,
        )
        .unwrap();

        let config = AiConfig::load(&state.0);
        assert_eq!(config.active_profile, "openai");
        assert_eq!(config.provider, Some(ProviderKind::OpenAI));
        assert_eq!(config.base_url, "https://api.openai.com/v1");
        assert_eq!(config.model, "gpt-4o-mini");
        let profile = config.profile("openai").expect("openai profile");
        assert_eq!(profile.api_key, "legacy-openai-key");
        assert_eq!(profile.base_url, "https://api.openai.com/v1");
        assert_eq!(profile.model, "gpt-4o-mini");
        assert_eq!(profile.name, "OpenAI");
        // The other builtins are present with their own (untouched) presets.
        assert_eq!(config.profiles.len(), 3);
        assert_eq!(
            config
                .profile("deepseek")
                .map(|profile| profile.model.clone()),
            Some("deepseek-flash".to_owned())
        );
        // Migration is persisted and idempotent.
        let text = std::fs::read_to_string(AiConfig::config_path(&state.0)).unwrap();
        assert!(text.contains("\"profiles\""), "{text}");
        assert!(text.contains("\"active_profile\": \"openai\""), "{text}");
        assert_eq!(AiConfig::load(&state.0), config);
    }

    #[test]
    fn legacy_custom_endpoint_becomes_a_custom_profile() {
        let state = TempDir::new("migrate-custom");
        std::fs::write(
            AiConfig::config_path(&state.0),
            r#"{"enabled":true,"base_url":"http://127.0.0.1:9999/v1","model":"my-model","api_key":"custom-key"}"#,
        )
        .unwrap();

        let config = AiConfig::load(&state.0);
        assert_eq!(config.active_profile, "custom-1");
        assert_eq!(config.resolved_provider(), ProviderKind::Custom);
        let profile = config.profile("custom-1").expect("custom profile");
        assert_eq!(profile.base_url, "http://127.0.0.1:9999/v1");
        assert_eq!(profile.model, "my-model");
        assert_eq!(profile.api_key, "custom-key");
        assert!(profile.kind == Some(ProviderKind::Custom));
        assert!(profile.models.is_empty());
        assert!(!profile.is_builtin());
        assert!(config.profiles().iter().filter(|p| p.is_builtin()).count() == 3);
    }

    #[test]
    fn activating_profiles_swaps_base_url_api_key_and_model_together() {
        let mut config = AiConfig {
            base_url: "https://proxy.example.com/v1".into(),
            model: "deepseek-v4-pro".into(),
            api_key: "deepseek-secret".into(),
            ..Default::default()
        };
        config.sync_active_profile();

        assert!(config.activate_profile("openai"));
        assert_eq!(config.active_profile, "openai");
        assert_eq!(config.base_url, "https://api.openai.com/v1");
        assert_eq!(config.model, "gpt-4.1");
        assert_eq!(config.api_key, "");
        assert_eq!(config.provider, Some(ProviderKind::OpenAI));

        config.api_key = "openai-secret".into();
        config.model = "gpt-4o-mini".into();
        config.sync_active_profile();

        // Switching back restores the first profile's endpoint/key/model.
        assert!(config.activate_profile("deepseek"));
        assert_eq!(config.base_url, "https://proxy.example.com/v1");
        assert_eq!(config.model, "deepseek-v4-pro");
        assert_eq!(config.api_key, "deepseek-secret");
        assert_eq!(config.resolved_provider(), ProviderKind::DeepSeek);

        let openai = config.profile("openai").expect("openai profile");
        assert_eq!(openai.api_key, "openai-secret");
        assert_eq!(openai.model, "gpt-4o-mini");
        assert_eq!(openai.base_url, "https://api.openai.com/v1");

        // Unknown / already-active ids do not switch.
        assert!(!config.activate_profile("nope"));
        assert!(!config.activate_profile("deepseek"));
    }

    #[test]
    fn custom_profiles_can_be_added_renamed_and_removed() {
        let mut config = AiConfig::default();
        let id = config.add_custom_profile();
        assert_eq!(id, "custom-1");
        assert!(!config.profile(&id).expect("custom").is_builtin());

        assert!(config.activate_profile(&id));
        config.base_url = "http://127.0.0.1:7777/v1".into();
        config.model = "local-model".into();
        config.api_key = "local-key".into();
        config.sync_active_profile();
        assert!(config.rename_active_profile("本机网关"));
        assert_eq!(
            config
                .profile(&id)
                .map(|profile| profile.display_name().to_owned()),
            Some("本机网关".to_owned())
        );
        assert!(!config.rename_active_profile("   "));

        // The custom profile is listed after the builtins.
        assert_eq!(config.profiles().len(), 4);
        assert_eq!(
            config.profiles().last().map(|profile| profile.id.as_str()),
            Some("custom-1")
        );

        assert!(config.remove_profile(&id));
        assert!(config.profile(&id).is_none());
        assert_eq!(config.active_profile, "deepseek");
        assert_eq!(config.base_url, ProviderKind::DeepSeek.default_base_url());
        // Builtin profiles are protected; unknown ids are rejected.
        assert!(!config.remove_profile("deepseek"));
        assert!(!config.remove_profile("missing"));
    }

    #[test]
    fn activating_a_missing_profile_is_rejected() {
        let mut config = AiConfig::default();
        config.active_profile.clear();
        assert!(!config.activate_profile(""));
        assert!(!config.activate_profile("openai-with-typo"));
        // Still the previous flat values.
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
    }

    #[test]
    fn apply_provider_switches_preset_endpoint_and_model() {
        let mut config = AiConfig {
            enabled: true,
            api_key: "k".into(),
            ..AiConfig::default()
        };

        config.apply_provider(ProviderKind::OpenAI);
        assert_eq!(config.provider, Some(ProviderKind::OpenAI));
        assert_eq!(config.base_url, "https://api.openai.com/v1");
        assert_eq!(config.model, "gpt-4.1");
        assert_eq!(config.resolved_provider(), ProviderKind::OpenAI);

        config.apply_provider(ProviderKind::SiliconFlow);
        assert_eq!(config.base_url, "https://api.siliconflow.cn/v1");
        assert_eq!(config.model, "deepseek-ai/DeepSeek-V3");

        config.apply_provider(ProviderKind::DeepSeek);
        assert_eq!(config.base_url, "https://api.deepseek.com/v1");
        assert_eq!(config.model, "deepseek-flash");

        // Custom never forces an endpoint: the user's values are preserved.
        config.base_url = "http://127.0.0.1:8080/v1".into();
        config.model = "my-own-model".into();
        config.apply_provider(ProviderKind::Custom);
        assert_eq!(config.provider, Some(ProviderKind::Custom));
        assert_eq!(config.base_url, "http://127.0.0.1:8080/v1");
        assert_eq!(config.model, "my-own-model");
        assert!(config.provider_preset().is_none());
        assert!(config.model_price().is_none());
    }

    #[test]
    fn cost_estimate_uses_the_bundled_price_catalog() {
        // Monday 2026-10-19 10:00 Beijing = 1792375200000 ms UTC = peak.
        let peak_ms = 1_792_375_200_000;
        let usage = TokenUsage {
            prompt_tokens: 1_000_000,
            completion_tokens: 1_000_000,
            total_tokens: 2_000_000,
            calls: 1,
            estimated: false,
        };
        let deepseek = AiConfig {
            enabled: true,
            provider: Some(ProviderKind::DeepSeek),
            model: "deepseek-flash".into(),
            ..AiConfig::default()
        };
        let estimate = deepseek.cost_estimate(&usage, peak_ms);
        assert!(estimate.priced);
        assert!(!estimate.off_peak);
        assert_eq!(estimate.cny, 10.0);
        assert_eq!(
            deepseek.price_note_zh().as_deref(),
            Some("≈ 估算 · 价目快照 2026-10-22")
        );

        let usd = AiConfig {
            enabled: true,
            provider: Some(ProviderKind::OpenAI),
            model: "gpt-4o-mini".into(),
            ..AiConfig::default()
        };
        let estimate = usd.cost_estimate(&usage, peak_ms);
        assert!((estimate.cny - 5.4).abs() < 1e-9, "{estimate:?}");
        assert_eq!(
            usd.price_note_zh().as_deref(),
            Some("≈ 估算 · 价目快照 2026-10-22 · USD→CNY 7.2")
        );

        let custom = AiConfig {
            enabled: true,
            provider: Some(ProviderKind::Custom),
            model: "deepseek-flash".into(),
            ..AiConfig::default()
        };
        assert!(!custom.cost_estimate(&usage, peak_ms).priced);
        assert!(custom.price_note_zh().is_none());
    }

    #[test]
    fn endpoint_joins_base_url() {
        let mut config = AiConfig::default();
        assert_eq!(
            config.endpoint(),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(
            config.models_endpoint(),
            "https://api.deepseek.com/v1/models"
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
    fn models_endpoint_normalizes_common_base_shapes() {
        let cases = [
            (
                "https://api.deepseek.com/v1",
                "https://api.deepseek.com/v1/models",
            ),
            (
                "https://api.deepseek.com/v1/",
                "https://api.deepseek.com/v1/models",
            ),
            (
                "https://api.deepseek.com/v1/models",
                "https://api.deepseek.com/v1/models",
            ),
            (
                "https://api.deepseek.com/v1/models/",
                "https://api.deepseek.com/v1/models",
            ),
            (
                "https://api.deepseek.com/v1/chat/completions",
                "https://api.deepseek.com/v1/models",
            ),
            (
                "  https://api.siliconflow.cn/v1  ",
                "https://api.siliconflow.cn/v1/models",
            ),
        ];
        for (base, expected) in cases {
            let config = AiConfig {
                base_url: base.to_owned(),
                ..Default::default()
            };
            assert_eq!(config.models_endpoint(), expected, "base={base:?}");
        }
        let empty = AiConfig {
            base_url: String::new(),
            ..Default::default()
        };
        assert_eq!(
            empty.models_endpoint(),
            format!("{DEFAULT_BASE_URL}{MODELS_PATH}")
        );
    }

    #[test]
    fn profile_model_lists_are_trimmed_deduped_and_sorted() {
        let mut profile = AiProfile::custom("custom-2", "测试");
        profile.set_models([
            " z-model ".to_owned(),
            "a-model".to_owned(),
            "A-Model".to_owned(),
            String::new(),
            "b-model".to_owned(),
        ]);
        assert_eq!(profile.models, ["a-model", "b-model", "z-model"]);
        assert!(profile.updated_ms > 0);
        profile.model = "A-MODEL".to_owned();
        assert!(profile.matches_model("A-MODEL"));
        assert!(!profile.matches_model("other"));
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
    fn debug_redacts_api_keys_everywhere() {
        let mut config = AiConfig {
            enabled: true,
            api_key: "super-secret-key".into(),
            ..AiConfig::default()
        };
        config.upsert_profile(AiProfile::custom("custom-1", "secret profile"));
        if let Some(profile) = config.profile_mut("custom-1") {
            profile.api_key = "profile-secret-key".into();
        }
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("super-secret-key"), "{rendered}");
        assert!(!rendered.contains("profile-secret-key"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        let profile = config.profile("custom-1").unwrap();
        let rendered = format!("{profile:?}");
        assert!(!rendered.contains("profile-secret-key"), "{rendered}");
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
            provider: Some(ProviderKind::Custom),
            ..AiConfig::default()
        };
        config.save(&state.0).unwrap();
        assert_eq!(AiConfig::config_path(&state.0), state.0.join("ai.json"));
        assert_eq!(AiConfig::load(&state.0), config);

        // The serialized layout carries both the flat active values and the
        // profile map.
        let text = std::fs::read_to_string(AiConfig::config_path(&state.0)).unwrap();
        for key in [
            "enabled",
            "base_url",
            "model",
            "api_key",
            "timeout_secs",
            "provider",
            "active_profile",
            "profiles",
        ] {
            assert!(
                text.contains(&format!("\"{key}\"")),
                "missing {key}: {text}"
            );
        }

        // Corrupted files fall back to defaults instead of erroring.
        std::fs::write(AiConfig::config_path(&state.0), "{not json").unwrap();
        assert_eq!(AiConfig::load(&state.0), AiConfig::default());
    }

    #[test]
    fn save_migrates_legacy_configs_before_writing() {
        let state = TempDir::new("save-migrates");
        // Simulate a legacy struct built in memory (no profiles at all).
        let mut config = AiConfig::default();
        config.profiles.clear();
        config.active_profile.clear();
        config.base_url = "https://api.openai.com/v1".into();
        config.model = "gpt-4o-mini".into();
        config.api_key = "legacy".into();
        config.provider = None;
        config.save(&state.0).unwrap();

        let text = std::fs::read_to_string(AiConfig::config_path(&state.0)).unwrap();
        assert!(text.contains("\"profiles\""), "{text}");
        let loaded = AiConfig::load(&state.0);
        assert_eq!(loaded.active_profile, "openai");
        assert_eq!(loaded.api_key, "legacy");
        assert_eq!(
            loaded
                .profile("openai")
                .map(|profile| profile.model.clone()),
            Some("gpt-4o-mini".to_owned())
        );
    }

    #[test]
    fn builtin_profile_ids_are_stable_and_protected() {
        assert_eq!(builtin_profile_ids(), ["deepseek", "openai", "siliconflow"]);
        for kind in ProviderKind::BUILTIN {
            assert!(is_builtin_profile_id(kind.id()));
            assert!(is_builtin_profile_id(&kind.id().to_ascii_uppercase()));
        }
        assert!(!is_builtin_profile_id("custom"));
        assert!(!is_builtin_profile_id("custom-1"));
        assert!(!is_builtin_profile_id(""));
    }
}

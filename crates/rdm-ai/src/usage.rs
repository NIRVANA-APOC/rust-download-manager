//! Token accounting for OpenAI-compatible AI calls.
//!
//! The GUI's resource monitor reports how many tokens AI parsing/classification
//! actually consumed. Providers that follow the OpenAI wire format return a
//! `usage` object on non-streaming completions; streaming replies may include a
//! final usage-only chunk when the request opts into `stream_options`.
//!
//! When a provider does not report usage at all, [`TokenUsage::estimate_from_text`]
//! produces a clearly-marked estimate (`estimated = true`) instead of pretending
//! the number is exact. Consumers must render estimated values with an `≈` and
//! explain it, never mix them silently into exact totals.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Token accounting for one or more model calls.
///
/// `calls` counts model calls (not HTTP attempts), so `total_tokens / calls`
/// remains meaningful. All arithmetic saturates: a broken provider can never
/// underflow the monitor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    /// Number of model calls contributing to this usage.
    #[serde(default)]
    pub calls: u32,
    /// `true` when at least one contributing call was estimated from text length
    /// because the provider did not return a `usage` object.
    #[serde(default)]
    pub estimated: bool,
}

impl TokenUsage {
    /// Zero usage (no calls).
    pub const ZERO: Self = Self {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        calls: 0,
        estimated: false,
    };

    pub fn is_zero(&self) -> bool {
        self.calls == 0 && self.total_tokens == 0
    }

    /// Add one call's usage into this accumulator (saturating).
    pub fn merge(&mut self, other: TokenUsage) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.calls = self.calls.saturating_add(other.calls);
        self.estimated |= other.estimated;
    }

    /// Read a provider `usage` object.
    ///
    /// Accepts the OpenAI shape (`prompt_tokens` / `completion_tokens` /
    /// `total_tokens`); when `total_tokens` is missing it is derived from the
    /// other two. A missing/invalid object returns `None` so callers can decide
    /// whether to estimate.
    pub fn from_json_usage(value: Option<&Value>) -> Option<Self> {
        let value = value?;
        if !value.is_object() {
            return None;
        }
        let prompt = u64_field(value, "prompt_tokens").unwrap_or(0);
        let completion = u64_field(value, "completion_tokens").unwrap_or(0);
        let total =
            u64_field(value, "total_tokens").unwrap_or_else(|| prompt.saturating_add(completion));
        if prompt == 0 && completion == 0 && total == 0 {
            return None;
        }
        Some(Self {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: total,
            calls: 1,
            estimated: false,
        })
    }

    /// Estimate one call from the prompt and reply text when the provider did
    /// not return usage. Marked [`TokenUsage::estimated`].
    pub fn estimate_from_text(prompt: &str, completion: &str) -> Self {
        let prompt_tokens = estimate_text_tokens(prompt);
        let completion_tokens = estimate_text_tokens(completion);
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens.saturating_add(completion_tokens),
            calls: 1,
            estimated: true,
        }
    }

    /// Exact prompt/completion copy when it is already known; `None` when a
    /// stream reported no usage and the caller must estimate instead.
    pub fn single(prompt_tokens: u64, completion_tokens: u64) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens.saturating_add(completion_tokens),
            calls: 1,
            estimated: false,
        }
    }
}

/// Rough token estimate for providers that do not return usage.
///
/// ASCII text averages ~4 characters per token; CJK text is closer to one token
/// per character. The result is intentionally conservative (never 0 for a
/// non-empty string) and is always labelled as approximate by the UI.
pub fn estimate_text_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let mut ascii: u64 = 0;
    let mut wide: u64 = 0;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii = ascii.saturating_add(1);
        } else {
            wide = wide.saturating_add(1);
        }
    }
    let ascii_tokens = ascii.saturating_add(3) / 4;
    ascii_tokens.saturating_add(wide).max(1)
}

fn u64_field(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(|field| {
        field
            .as_u64()
            .or_else(|| field.as_i64().and_then(|raw| u64::try_from(raw).ok()))
            .or_else(|| field.as_f64().map(|raw| raw.max(0.0) as u64))
            .or_else(|| {
                // Some gateways serialize counters as decimal strings.
                field
                    .as_str()
                    .and_then(|raw| raw.trim().parse::<u64>().ok())
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_the_openai_usage_shape() {
        let usage = TokenUsage::from_json_usage(Some(&json!({
            "prompt_tokens": 120,
            "completion_tokens": 30,
            "total_tokens": 150,
            "prompt_tokens_details": {"cached_tokens": 64}
        })))
        .expect("usage");
        assert_eq!(usage.prompt_tokens, 120);
        assert_eq!(usage.completion_tokens, 30);
        assert_eq!(usage.total_tokens, 150);
        assert_eq!(usage.calls, 1);
        assert!(!usage.estimated);
    }

    #[test]
    fn derives_missing_total_and_tolerates_numeric_strings() {
        let usage = TokenUsage::from_json_usage(Some(&json!({
            "prompt_tokens": 7,
            "completion_tokens": "5"
        })))
        .expect("usage");
        assert_eq!(usage.total_tokens, 12);
    }

    #[test]
    fn rejects_empty_or_non_object_usage() {
        assert!(TokenUsage::from_json_usage(None).is_none());
        assert!(TokenUsage::from_json_usage(Some(&json!(null))).is_none());
        assert!(TokenUsage::from_json_usage(Some(&json!({"prompt_tokens": 0}))).is_none());
        assert!(TokenUsage::from_json_usage(Some(&json!([1, 2]))).is_none());
    }

    #[test]
    fn merging_saturates_and_propagates_estimation() {
        let mut total = TokenUsage::single(10, 5);
        total.merge(TokenUsage {
            prompt_tokens: 3,
            completion_tokens: 2,
            total_tokens: 5,
            calls: 1,
            estimated: true,
        });
        assert_eq!(total.prompt_tokens, 13);
        assert_eq!(total.completion_tokens, 7);
        assert_eq!(total.total_tokens, 20);
        assert_eq!(total.calls, 2);
        assert!(total.estimated);
        assert!(!total.is_zero());

        let mut saturated = TokenUsage {
            prompt_tokens: u64::MAX,
            ..TokenUsage::ZERO
        };
        saturated.merge(TokenUsage::single(1, 1));
        assert_eq!(saturated.prompt_tokens, u64::MAX);
    }

    #[test]
    fn estimates_ascii_and_cjk() {
        // 8 ASCII chars ≈ 2 tokens; 4 CJK chars = 4 tokens.
        let usage = TokenUsage::estimate_from_text("abcdefgh", "中文标题");
        assert_eq!(usage.prompt_tokens, 2);
        assert_eq!(usage.completion_tokens, 4);
        assert_eq!(usage.total_tokens, 6);
        assert!(usage.estimated);
        assert_eq!(estimate_text_tokens(""), 0);
        assert_eq!(estimate_text_tokens("a"), 1);
    }
}

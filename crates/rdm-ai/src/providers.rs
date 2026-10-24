//! LLM provider presets, model price catalog and "≈ estimated cost" math
//! (`docs/design/14-llm-provider-pricing.md`).
//!
//! Prices are a **point-in-time public snapshot** ([`PRICE_CATALOG_DATE`]), not a
//! billing contract: the resource monitor only uses them for a clearly marked
//! estimate. The formula is
//! `prompt_tokens × input_price + completion_tokens × output_price`; provider-side
//! prompt-cache hits are deliberately not modelled, so the estimate is a known
//! upper bound for cached prompts (the UI explains this).
//!
//! DeepSeek charges less outside its Beijing-time peak windows (weekday
//! 09:00–12:00 and 14:00–18:00). Public holidays are **not** modelled — they are
//! treated like ordinary weekdays, as stated in [`deepseek_peak_note_zh`].
//!
//! No network access, no additional dependencies: the whole catalog is static.

use serde::{Deserialize, Serialize};

use crate::usage::TokenUsage;

/// Date of the bundled public price snapshot (YYYY-MM-DD).
pub const PRICE_CATALOG_DATE: &str = "2026-10-22";
/// Fixed USD→CNY rate used to normalize USD catalogs into the monitor's CNY
/// display. A constant (not a live FX lookup) keeps the estimate reproducible.
pub const USD_TO_CNY: f64 = 7.2;

const MS_PER_DAY: u64 = 24 * 60 * 60 * 1000;
const MS_PER_HOUR: u64 = 60 * 60 * 1000;
/// Beijing is UTC+8 all year (no daylight saving).
const BEIJING_OFFSET_MS: u64 = 8 * MS_PER_HOUR;

/// One supported OpenAI-compatible provider family.
///
/// Serialized with explicit lowercase ids (`"deepseek"`, `"openai"`,
/// `"siliconflow"`, `"custom"`) so `ai.json` stays readable and matches
/// [`ProviderKind::id`] (`snake_case` would produce `deep_seek`/`open_a_i`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ProviderKind {
    #[default]
    #[serde(rename = "deepseek")]
    DeepSeek,
    #[serde(rename = "openai")]
    OpenAI,
    #[serde(rename = "siliconflow")]
    SiliconFlow,
    #[serde(rename = "custom")]
    Custom,
}

impl ProviderKind {
    /// Every variant in display order.
    pub const ALL: [ProviderKind; 4] = [
        Self::DeepSeek,
        Self::OpenAI,
        Self::SiliconFlow,
        Self::Custom,
    ];

    /// Bundled provider families (everything except [`ProviderKind::Custom`]),
    /// in display order. One builtin profile exists per entry
    /// (`docs/design/16-reuse-and-simplification.md` §5).
    pub const BUILTIN: [ProviderKind; 3] = [Self::DeepSeek, Self::OpenAI, Self::SiliconFlow];

    /// Stable wire/persistence id (also used by the GUI chips).
    pub fn id(self) -> &'static str {
        match self {
            Self::DeepSeek => "deepseek",
            Self::OpenAI => "openai",
            Self::SiliconFlow => "siliconflow",
            Self::Custom => "custom",
        }
    }

    /// Chinese label for the settings UI.
    pub fn label_zh(self) -> &'static str {
        match self {
            Self::DeepSeek => "DeepSeek",
            Self::OpenAI => "OpenAI",
            Self::SiliconFlow => "SiliconFlow",
            Self::Custom => "自定义",
        }
    }

    /// Default OpenAI-compatible base URL (`""` for a custom endpoint).
    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::DeepSeek => "https://api.deepseek.com/v1",
            Self::OpenAI => "https://api.openai.com/v1",
            Self::SiliconFlow => "https://api.siliconflow.cn/v1",
            Self::Custom => "",
        }
    }

    /// Default model (`""` for a custom endpoint: the user must type one).
    pub fn default_model(self) -> &'static str {
        match self {
            Self::DeepSeek => "deepseek-flash",
            Self::OpenAI => "gpt-4.1",
            Self::SiliconFlow => "deepseek-ai/DeepSeek-V3",
            Self::Custom => "",
        }
    }

    /// Parse a persisted/wire id (trimmed, case-insensitive).
    pub fn from_id(id: &str) -> Option<Self> {
        let id = id.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|kind| kind.id() == id)
    }

    /// Infer the provider from a base URL host; `None` when it is a custom
    /// (proxy/mock) endpoint.
    pub fn infer_from_base_url(base_url: &str) -> Option<Self> {
        let lower = base_url.trim().to_ascii_lowercase();
        if lower.contains("deepseek.") {
            Some(Self::DeepSeek)
        } else if lower.contains("openai.") {
            Some(Self::OpenAI)
        } else if lower.contains("siliconflow.") {
            Some(Self::SiliconFlow)
        } else {
            None
        }
    }

    /// `true` for the bundled provider families (not [`ProviderKind::Custom`]).
    pub fn is_builtin(self) -> bool {
        matches!(self, Self::DeepSeek | Self::OpenAI | Self::SiliconFlow)
    }

    /// Model ids of the bundled price catalog (`[]` for a custom endpoint).
    ///
    /// These seed a builtin profile's model list before the first live
    /// `GET /models` fetch; the live list then replaces them.
    pub fn catalog_model_ids(self) -> Vec<String> {
        preset(self)
            .map(|preset| {
                preset
                    .models
                    .iter()
                    .map(|price| price.model.to_owned())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Currency of a price catalog entry (and of a [`CostEstimate`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Currency {
    Cny,
    Usd,
}

impl Currency {
    /// Display symbol (`¥` / `$`).
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Cny => "¥",
            Self::Usd => "$",
        }
    }
}

/// One model's price per million tokens.
#[derive(Debug, Clone, Copy)]
pub struct ModelPrice {
    /// Model id sent in the request body (also what `ai.json` stores).
    pub model: &'static str,
    /// Human-readable label for the settings page.
    pub label: &'static str,
    pub currency: Currency,
    /// Input (prompt) price per 1M tokens, in [`ModelPrice::currency`].
    pub input_per_million: f64,
    /// Output (completion) price per 1M tokens, in [`ModelPrice::currency`].
    pub output_per_million: f64,
    /// `Some(0.5)` for providers with off-peak pricing (DeepSeek); `None` when
    /// the price is flat all day.
    pub off_peak_multiplier: Option<f64>,
}

/// One provider's preset: endpoint, price source and model catalog.
#[derive(Debug, Clone, Copy)]
pub struct ProviderPreset {
    pub kind: ProviderKind,
    pub base_url: &'static str,
    /// Public pricing page used for the snapshot.
    pub docs_url: &'static str,
    /// Snapshot date of the prices (`YYYY-MM-DD`).
    pub price_as_of: &'static str,
    pub models: &'static [ModelPrice],
    /// One-line Chinese note shown under the model list.
    pub pricing_note_zh: &'static str,
}

const DEEPSEEK_MODELS: &[ModelPrice] = &[
    ModelPrice {
        model: "deepseek-flash",
        label: "DeepSeek V4.1-Flash",
        currency: Currency::Cny,
        input_per_million: 2.0,
        output_per_million: 8.0,
        off_peak_multiplier: Some(0.5),
    },
    ModelPrice {
        model: "deepseek-v4-pro",
        label: "DeepSeek V4-Pro",
        currency: Currency::Cny,
        input_per_million: 9.0,
        output_per_million: 27.0,
        off_peak_multiplier: Some(0.5),
    },
];

const OPENAI_MODELS: &[ModelPrice] = &[
    ModelPrice {
        model: "gpt-4.1",
        label: "GPT-4.1",
        currency: Currency::Usd,
        input_per_million: 2.00,
        output_per_million: 8.00,
        off_peak_multiplier: None,
    },
    ModelPrice {
        model: "gpt-4.1-mini",
        label: "GPT-4.1 mini",
        currency: Currency::Usd,
        input_per_million: 0.40,
        output_per_million: 1.60,
        off_peak_multiplier: None,
    },
    ModelPrice {
        model: "gpt-4.1-nano",
        label: "GPT-4.1 nano",
        currency: Currency::Usd,
        input_per_million: 0.10,
        output_per_million: 0.40,
        off_peak_multiplier: None,
    },
    ModelPrice {
        model: "gpt-4o",
        label: "GPT-4o",
        currency: Currency::Usd,
        input_per_million: 2.50,
        output_per_million: 10.00,
        off_peak_multiplier: None,
    },
    ModelPrice {
        model: "gpt-4o-mini",
        label: "GPT-4o mini",
        currency: Currency::Usd,
        input_per_million: 0.15,
        output_per_million: 0.60,
        off_peak_multiplier: None,
    },
    ModelPrice {
        model: "o3-mini",
        label: "o3-mini",
        currency: Currency::Usd,
        input_per_million: 1.10,
        output_per_million: 4.40,
        off_peak_multiplier: None,
    },
];

const SILICONFLOW_MODELS: &[ModelPrice] = &[
    ModelPrice {
        model: "deepseek-ai/DeepSeek-V3",
        label: "DeepSeek-V3",
        currency: Currency::Cny,
        input_per_million: 2.0,
        output_per_million: 8.0,
        off_peak_multiplier: None,
    },
    ModelPrice {
        model: "deepseek-ai/DeepSeek-R1",
        label: "DeepSeek-R1",
        currency: Currency::Cny,
        input_per_million: 4.0,
        output_per_million: 16.0,
        off_peak_multiplier: None,
    },
    ModelPrice {
        model: "Qwen/Qwen2.5-72B-Instruct",
        label: "Qwen2.5-72B-Instruct",
        currency: Currency::Cny,
        input_per_million: 1.26,
        output_per_million: 1.26,
        off_peak_multiplier: None,
    },
    ModelPrice {
        model: "Qwen/Qwen2.5-7B-Instruct",
        label: "Qwen2.5-7B-Instruct",
        currency: Currency::Cny,
        input_per_million: 0.35,
        output_per_million: 0.35,
        off_peak_multiplier: None,
    },
];

static DEEPSEEK_PRESET: ProviderPreset = ProviderPreset {
    kind: ProviderKind::DeepSeek,
    base_url: "https://api.deepseek.com/v1",
    docs_url: "https://api-docs.deepseek.com/zh-cn/quick_start/pricing/",
    price_as_of: PRICE_CATALOG_DATE,
    models: DEEPSEEK_MODELS,
    pricing_note_zh: "人民币价目快照；高峰为北京时间工作日 09:00–12:00、14:00–18:00，\
                       空闲时段五折；法定节假日未单独建模（按工作日计）。",
};

static OPENAI_PRESET: ProviderPreset = ProviderPreset {
    kind: ProviderKind::OpenAI,
    base_url: "https://api.openai.com/v1",
    docs_url: "https://openai.com/api/pricing/",
    price_as_of: PRICE_CATALOG_DATE,
    models: OPENAI_MODELS,
    pricing_note_zh: "美元价目快照，费用按 1 USD = 7.2 CNY 折算为人民币估算。",
};

static SILICONFLOW_PRESET: ProviderPreset = ProviderPreset {
    kind: ProviderKind::SiliconFlow,
    base_url: "https://api.siliconflow.cn/v1",
    docs_url: "https://siliconflow.cn/pricing",
    price_as_of: PRICE_CATALOG_DATE,
    models: SILICONFLOW_MODELS,
    pricing_note_zh: "人民币价目快照（第三方公开价目），无峰谷差异。",
};

/// The preset of `kind`; `None` for [`ProviderKind::Custom`] (no bundled
/// endpoint/models/prices).
pub fn preset(kind: ProviderKind) -> Option<&'static ProviderPreset> {
    match kind {
        ProviderKind::DeepSeek => Some(&DEEPSEEK_PRESET),
        ProviderKind::OpenAI => Some(&OPENAI_PRESET),
        ProviderKind::SiliconFlow => Some(&SILICONFLOW_PRESET),
        ProviderKind::Custom => None,
    }
}

/// Look up one model's price (trimmed, case-insensitive).
///
/// Legacy DeepSeek aliases `deepseek-chat` / `deepseek-reasoner` (the pre-V4
/// names) map onto `deepseek-flash` / `deepseek-v4-pro` prices, so configs saved
/// before this catalog still price correctly.
pub fn find_model(kind: ProviderKind, model: &str) -> Option<&'static ModelPrice> {
    let preset = preset(kind)?;
    let wanted = model.trim();
    if wanted.is_empty() {
        return None;
    }
    if let Some(price) = preset
        .models
        .iter()
        .find(|price| price.model.eq_ignore_ascii_case(wanted))
    {
        return Some(price);
    }
    let alias = if kind == ProviderKind::DeepSeek {
        deepseek_legacy_alias(wanted)?
    } else {
        return None;
    };
    preset
        .models
        .iter()
        .find(|price| price.model.eq_ignore_ascii_case(alias))
}

fn deepseek_legacy_alias(model: &str) -> Option<&'static str> {
    match model.to_ascii_lowercase().as_str() {
        "deepseek-chat" => Some("deepseek-flash"),
        "deepseek-reasoner" => Some("deepseek-v4-pro"),
        _ => None,
    }
}

/// Estimated cost of one token usage under one provider/model.
#[derive(Debug, Clone, Copy)]
pub struct CostEstimate {
    /// Estimated cost converted to CNY (the monitor's display currency).
    pub cny: f64,
    /// Estimated cost in the catalog's native currency.
    pub native: f64,
    /// Currency of [`CostEstimate::native`].
    pub currency: Currency,
    /// `true` when the off-peak (discounted) price was applied.
    pub off_peak: bool,
    /// `false` when the model is unknown or the provider has no catalog
    /// ([`ProviderKind::Custom`]): `cny`/`native` are then `0.0` and the UI must
    /// say "no price catalog" instead of showing a fake `¥0`.
    pub priced: bool,
    /// Snapshot date of the prices used.
    pub catalog_date: &'static str,
}

/// Estimate the cost of `usage` for `model` at wall-clock time `at_ms`
/// (Unix epoch milliseconds, UTC).
///
/// Unknown models and [`ProviderKind::Custom`] are unpriced (`cny = 0`,
/// `priced = false`). DeepSeek uses its off-peak multiplier outside the Beijing
/// weekday peak windows. USD catalogs are converted with [`USD_TO_CNY`].
pub fn estimate_cost(
    kind: ProviderKind,
    model: &str,
    usage: &TokenUsage,
    at_ms: u64,
) -> CostEstimate {
    let Some(price) = find_model(kind, model) else {
        return CostEstimate {
            cny: 0.0,
            native: 0.0,
            currency: provider_currency(kind),
            off_peak: false,
            priced: false,
            catalog_date: PRICE_CATALOG_DATE,
        };
    };

    let off_peak = price.off_peak_multiplier.is_some() && deepseek_is_off_peak(at_ms);
    let mut input_price = price.input_per_million;
    let mut output_price = price.output_per_million;
    if off_peak {
        if let Some(multiplier) = price.off_peak_multiplier {
            input_price *= multiplier;
            output_price *= multiplier;
        }
    }

    let million = 1_000_000.0;
    let native = (usage.prompt_tokens as f64 / million) * input_price
        + (usage.completion_tokens as f64 / million) * output_price;
    let cny = match price.currency {
        Currency::Cny => native,
        Currency::Usd => native * USD_TO_CNY,
    };
    CostEstimate {
        cny,
        native,
        currency: price.currency,
        off_peak,
        priced: true,
        catalog_date: PRICE_CATALOG_DATE,
    }
}

fn provider_currency(kind: ProviderKind) -> Currency {
    match kind {
        ProviderKind::OpenAI => Currency::Usd,
        ProviderKind::DeepSeek | ProviderKind::SiliconFlow | ProviderKind::Custom => Currency::Cny,
    }
}

/// `true` when DeepSeek's off-peak (discounted) price applies at `at_ms`.
///
/// Peak is **Beijing time (UTC+8) Monday–Friday 09:00–12:00 and 14:00–18:00**
/// (start inclusive, end exclusive); everything else — including all weekend
/// hours — is off-peak. Public holidays are not modelled.
pub fn deepseek_is_off_peak(at_ms: u64) -> bool {
    let beijing_ms = at_ms.saturating_add(BEIJING_OFFSET_MS);
    let day_ms = beijing_ms % MS_PER_DAY;
    let days_since_epoch = beijing_ms / MS_PER_DAY;
    // 1970-01-01 was a Thursday. 0 = Sunday … 6 = Saturday.
    let weekday = (days_since_epoch + 4) % 7;
    let weekday_peak = (1..=5).contains(&weekday);
    let morning_peak = (9 * MS_PER_HOUR..12 * MS_PER_HOUR).contains(&day_ms);
    let afternoon_peak = (14 * MS_PER_HOUR..18 * MS_PER_HOUR).contains(&day_ms);
    !(weekday_peak && (morning_peak || afternoon_peak))
}

/// Chinese note about DeepSeek's peak windows (shown in settings/monitor).
pub fn deepseek_peak_note_zh() -> &'static str {
    "高峰：北京时间周一至周五 09:00–12:00、14:00–18:00（周末全天低价，法定节假日未单独建模）"
}

/// One-line Chinese price label for a model chip, e.g.
/// `"输入 ¥2/M · 输出 ¥8/M · 高峰价，空闲时段五折"`.
pub fn price_label_zh(price: &ModelPrice) -> String {
    let symbol = price.currency.symbol();
    let base = format!(
        "输入 {symbol}{}/M · 输出 {symbol}{}/M",
        format_price_number(price.input_per_million),
        format_price_number(price.output_per_million)
    );
    match price.off_peak_multiplier {
        Some(0.5) => format!("{base} · 高峰价，空闲时段五折"),
        Some(multiplier) => format!("{base} · 高峰价，空闲时段 {multiplier} 倍"),
        None => format!("{base} · 统一价，无峰谷"),
    }
}

/// Compact one-line price hint for a model list row, e.g. `"¥2 / ¥8 /M"`.
///
/// `price_label_zh` is too long for a dense settings list; this keeps the
/// per-million input/output figures only.
pub fn price_short_zh(price: &ModelPrice) -> String {
    let symbol = price.currency.symbol();
    format!(
        "{symbol}{}/{symbol}{}/M",
        format_price_number(price.input_per_million),
        format_price_number(price.output_per_million)
    )
}

/// Format a per-million price without trailing zeros (`2.0` → `"2"`,
/// `0.60` → `"0.6"`, `1.26` → `"1.26"`).
fn format_price_number(value: f64) -> String {
    if !value.is_finite() {
        return "0".to_owned();
    }
    if (value - value.round()).abs() < 1e-9 {
        return format!("{value:.0}");
    }
    let mut text = format!("{value}");
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unix epoch milliseconds of a Beijing (UTC+8) local civil time.
    fn beijing_ms(year: i64, month: u32, day: u32, hour: u32, minute: u32) -> u64 {
        let utc_ms = days_from_civil(year, month, day) * MS_PER_DAY as i64
            + (hour as i64 * 3600 + minute as i64 * 60) * 1000
            - BEIJING_OFFSET_MS as i64;
        utc_ms.max(0) as u64
    }

    /// Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
    fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
        let year = if month <= 2 { year - 1 } else { year };
        let era = if year >= 0 { year } else { year - 399 } / 400;
        let year_of_era = year - era * 400;
        let month_prime = (month as i64 + 9) % 12;
        let day_of_year = (153 * month_prime + 2) / 5 + day as i64 - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        era * 146_097 + day_of_era - 719_468
    }

    fn million_usage() -> TokenUsage {
        TokenUsage {
            prompt_tokens: 1_000_000,
            completion_tokens: 1_000_000,
            total_tokens: 2_000_000,
            calls: 1,
            estimated: false,
        }
    }

    /// 2026-10-19 is a Monday, 2026-10-24/25 the following weekend.
    fn monday(hour: u32, minute: u32) -> u64 {
        beijing_ms(2026, 10, 19, hour, minute)
    }

    #[test]
    fn deepseek_peak_boundaries_follow_beijing_weekday_windows() {
        assert!(deepseek_is_off_peak(monday(8, 59)));
        assert!(!deepseek_is_off_peak(monday(9, 0)));
        assert!(!deepseek_is_off_peak(monday(11, 59)));
        assert!(deepseek_is_off_peak(monday(12, 0)));
        assert!(deepseek_is_off_peak(monday(13, 59)));
        assert!(!deepseek_is_off_peak(monday(14, 0)));
        assert!(!deepseek_is_off_peak(monday(17, 59)));
        assert!(deepseek_is_off_peak(monday(18, 0)));
        // Weekend is off-peak even inside the weekday peak windows.
        assert!(deepseek_is_off_peak(beijing_ms(2026, 10, 24, 10, 0)));
        assert!(deepseek_is_off_peak(beijing_ms(2026, 10, 25, 15, 30)));
        // Friday 2026-10-23 is still a weekday.
        assert!(!deepseek_is_off_peak(beijing_ms(2026, 10, 23, 10, 0)));
    }

    #[test]
    fn deepseek_flash_costs_peak_and_off_peak_prices() {
        let usage = million_usage();
        let peak = estimate_cost(
            ProviderKind::DeepSeek,
            "deepseek-flash",
            &usage,
            monday(10, 0),
        );
        assert!(peak.priced);
        assert_eq!(peak.currency, Currency::Cny);
        assert!(!peak.off_peak);
        assert_eq!(peak.native, 10.0);
        assert_eq!(peak.cny, 10.0);
        assert_eq!(peak.catalog_date, PRICE_CATALOG_DATE);

        let off_peak = estimate_cost(
            ProviderKind::DeepSeek,
            "deepseek-flash",
            &usage,
            monday(20, 0),
        );
        assert!(off_peak.priced);
        assert!(off_peak.off_peak);
        assert_eq!(off_peak.native, 5.0);
        assert_eq!(off_peak.cny, 5.0);
    }

    #[test]
    fn deepseek_pro_and_openai_usd_prices_convert_to_cny() {
        let usage = million_usage();
        let pro = estimate_cost(
            ProviderKind::DeepSeek,
            "deepseek-v4-pro",
            &usage,
            monday(10, 0),
        );
        assert_eq!(pro.native, 36.0);
        assert_eq!(pro.cny, 36.0);

        let openai = estimate_cost(ProviderKind::OpenAI, "gpt-4o-mini", &usage, monday(10, 0));
        assert!(openai.priced);
        assert_eq!(openai.currency, Currency::Usd);
        assert!(!openai.off_peak, "OpenAI has flat pricing");
        assert!((openai.native - 0.75).abs() < 1e-9, "{openai:?}");
        assert!((openai.cny - 5.4).abs() < 1e-9, "{openai:?}");
    }

    #[test]
    fn legacy_deepseek_names_and_case_are_priced() {
        let flash = find_model(ProviderKind::DeepSeek, " deepseek-chat ").expect("alias price");
        assert_eq!(flash.model, "deepseek-flash");
        let pro = find_model(ProviderKind::DeepSeek, "DeepSeek-Reasoner").expect("alias price");
        assert_eq!(pro.model, "deepseek-v4-pro");
        assert_eq!(
            find_model(ProviderKind::DeepSeek, "DEEPSEEK-FLASH")
                .expect("case-insensitive")
                .model,
            "deepseek-flash"
        );
        // Aliases only apply to DeepSeek; other providers must not borrow them.
        assert!(find_model(ProviderKind::OpenAI, "deepseek-chat").is_none());
    }

    #[test]
    fn unknown_models_and_custom_providers_are_unpriced() {
        let usage = million_usage();
        let unknown = estimate_cost(ProviderKind::DeepSeek, "not-a-model", &usage, monday(10, 0));
        assert!(!unknown.priced);
        assert_eq!(unknown.cny, 0.0);
        assert_eq!(unknown.native, 0.0);

        let empty = estimate_cost(ProviderKind::DeepSeek, "  ", &usage, monday(10, 0));
        assert!(!empty.priced);

        let custom = estimate_cost(
            ProviderKind::Custom,
            "deepseek-flash",
            &usage,
            monday(10, 0),
        );
        assert!(!custom.priced, "custom endpoints have no bundled catalog");
        assert_eq!(custom.cny, 0.0);
        assert!(preset(ProviderKind::Custom).is_none());
    }

    #[test]
    fn presets_cover_every_non_custom_provider() {
        assert_eq!(ProviderKind::ALL.len(), 4);
        for kind in ProviderKind::ALL {
            assert!(!kind.id().is_empty(), "{kind:?}");
            assert!(!kind.label_zh().is_empty(), "{kind:?}");
            assert_eq!(ProviderKind::from_id(kind.id()), Some(kind));
            match preset(kind) {
                Some(preset) => {
                    assert_eq!(preset.kind, kind);
                    assert_eq!(preset.price_as_of, PRICE_CATALOG_DATE);
                    assert!(!preset.docs_url.is_empty());
                    assert!(!preset.models.is_empty());
                    assert!(!preset.pricing_note_zh.is_empty());
                    assert_eq!(preset.base_url, kind.default_base_url());
                    assert_eq!(preset.models[0].model, kind.default_model());
                    // Model ids are unique within one catalog.
                    let mut ids: Vec<&str> = preset.models.iter().map(|p| p.model).collect();
                    ids.sort_unstable();
                    ids.dedup();
                    assert_eq!(ids.len(), preset.models.len(), "{kind:?}");
                }
                None => {
                    assert_eq!(kind, ProviderKind::Custom);
                    assert!(kind.default_base_url().is_empty());
                    assert!(kind.default_model().is_empty());
                }
            }
        }
    }

    #[test]
    fn provider_serde_ids_match_the_stable_ids() {
        for kind in ProviderKind::ALL {
            let encoded = serde_json::to_string(&kind).expect("serialize provider");
            assert_eq!(encoded, format!("\"{}\"", kind.id()));
            let decoded: ProviderKind = serde_json::from_str(&encoded).expect("deserialize");
            assert_eq!(decoded, kind);
        }
        for (currency, wire) in [(Currency::Cny, "\"cny\""), (Currency::Usd, "\"usd\"")] {
            assert_eq!(serde_json::to_string(&currency).unwrap(), wire);
        }
    }

    #[test]
    fn provider_is_inferred_from_known_base_urls_only() {
        assert_eq!(
            ProviderKind::infer_from_base_url("https://api.deepseek.com/v1"),
            Some(ProviderKind::DeepSeek)
        );
        assert_eq!(
            ProviderKind::infer_from_base_url("https://api.openai.com/v1"),
            Some(ProviderKind::OpenAI)
        );
        assert_eq!(
            ProviderKind::infer_from_base_url("https://api.siliconflow.cn/v1"),
            Some(ProviderKind::SiliconFlow)
        );
        assert_eq!(
            ProviderKind::infer_from_base_url("http://127.0.0.1:8080/v1"),
            None
        );
    }

    #[test]
    fn price_labels_are_human_readable() {
        let flash = find_model(ProviderKind::DeepSeek, "deepseek-flash").unwrap();
        assert_eq!(
            price_label_zh(flash),
            "输入 ¥2/M · 输出 ¥8/M · 高峰价，空闲时段五折"
        );
        let mini = find_model(ProviderKind::OpenAI, "gpt-4o-mini").unwrap();
        assert_eq!(
            price_label_zh(mini),
            "输入 $0.15/M · 输出 $0.6/M · 统一价，无峰谷"
        );
        let qwen = find_model(ProviderKind::SiliconFlow, "Qwen/Qwen2.5-72B-Instruct").unwrap();
        assert_eq!(
            price_label_zh(qwen),
            "输入 ¥1.26/M · 输出 ¥1.26/M · 统一价，无峰谷"
        );
    }

    #[test]
    fn builtin_provider_helpers_cover_profiles_and_catalogs() {
        assert_eq!(ProviderKind::BUILTIN.len(), 3);
        assert!(ProviderKind::BUILTIN.iter().all(|kind| kind.is_builtin()));
        assert!(!ProviderKind::Custom.is_builtin());
        for kind in ProviderKind::BUILTIN {
            assert_eq!(
                kind.catalog_model_ids(),
                preset(kind)
                    .expect("builtin preset")
                    .models
                    .iter()
                    .map(|price| price.model.to_owned())
                    .collect::<Vec<_>>(),
                "{kind:?}"
            );
        }
        assert!(ProviderKind::Custom.catalog_model_ids().is_empty());

        let flash = find_model(ProviderKind::DeepSeek, "deepseek-flash").unwrap();
        assert_eq!(price_short_zh(flash), "¥2/¥8/M");
        let mini = find_model(ProviderKind::OpenAI, "gpt-4o-mini").unwrap();
        assert_eq!(price_short_zh(mini), "$0.15/$0.6/M");
    }

    #[test]
    fn peak_note_mentions_beijing_windows_and_holiday_limit() {
        let note = deepseek_peak_note_zh();
        assert!(note.contains("北京时间"), "{note}");
        assert!(note.contains("09:00–12:00"), "{note}");
        assert!(note.contains("14:00–18:00"), "{note}");
        assert!(note.contains("法定节假日"), "{note}");
    }

    #[test]
    fn cost_scales_linearly_with_tokens() {
        let quarter = TokenUsage {
            prompt_tokens: 250_000,
            completion_tokens: 250_000,
            total_tokens: 500_000,
            calls: 1,
            estimated: false,
        };
        let estimate = estimate_cost(
            ProviderKind::DeepSeek,
            "deepseek-flash",
            &quarter,
            monday(10, 0),
        );
        assert!((estimate.cny - 2.5).abs() < 1e-9, "{estimate:?}");

        let empty = estimate_cost(
            ProviderKind::DeepSeek,
            "deepseek-flash",
            &TokenUsage::ZERO,
            monday(10, 0),
        );
        assert!(empty.priced);
        assert_eq!(empty.cny, 0.0);
    }
}

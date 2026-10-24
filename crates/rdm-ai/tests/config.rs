//! Integration tests for `AiConfig` persistence and environment handling.

mod support;

use rdm_ai::AiConfig;
use support::TempDir;

#[test]
fn endpoint_join_variants() {
    let mut config = AiConfig::default();
    assert_eq!(
        config.endpoint(),
        "https://api.deepseek.com/v1/chat/completions"
    );

    config.base_url = "http://127.0.0.1:9000/v1/".to_owned();
    assert_eq!(
        config.endpoint(),
        "http://127.0.0.1:9000/v1/chat/completions"
    );

    config.base_url = "http://127.0.0.1:9000/v1/chat/completions".to_owned();
    assert_eq!(
        config.endpoint(),
        "http://127.0.0.1:9000/v1/chat/completions"
    );

    config.base_url = " http://127.0.0.1:9000/v1 ".to_owned();
    assert_eq!(
        config.endpoint(),
        "http://127.0.0.1:9000/v1/chat/completions"
    );
}

#[test]
fn save_load_roundtrip_in_state_dir() {
    let state = TempDir::new("config-roundtrip");
    assert_eq!(AiConfig::load(state.path()), AiConfig::default());

    let config = AiConfig {
        enabled: true,
        base_url: "http://127.0.0.1:1234/v1".to_owned(),
        model: "some-model".to_owned(),
        api_key: "stored-key".to_owned(),
        timeout_secs: 9,
        provider: None,
        ..AiConfig::default()
    };
    config.save(state.path()).unwrap();
    assert!(AiConfig::config_path(state.path()).is_file());
    assert_eq!(AiConfig::load(state.path()), config);

    // The file is valid JSON with the documented field names.
    let text = std::fs::read_to_string(AiConfig::config_path(state.path())).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
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
        assert!(value.get(key).is_some(), "missing field {key}: {text}");
    }
}

#[test]
fn legacy_ai_json_is_migrated_and_profile_switches_swap_all_three_fields() {
    let state = TempDir::new("config-profiles");
    // Old format: flat fields only, no profiles.
    std::fs::write(
        AiConfig::config_path(state.path()),
        r#"{"enabled":true,"base_url":"https://api.openai.com/v1","model":"gpt-4o-mini","api_key":"legacy-openai"}"#,
    )
    .unwrap();

    let mut config = AiConfig::load(state.path());
    assert_eq!(config.active_profile, "openai");
    assert_eq!(config.base_url, "https://api.openai.com/v1");
    assert_eq!(config.api_key, "legacy-openai");
    assert_eq!(config.model, "gpt-4o-mini");

    // Switching writes the flat values back, then loads the target profile.
    assert!(config.activate_profile("deepseek"));
    assert_eq!(config.base_url, "https://api.deepseek.com/v1");
    assert_eq!(config.model, "deepseek-flash");
    assert_eq!(config.api_key, "");
    config.api_key = "new-deepseek-key".to_owned();
    config.sync_active_profile();

    assert!(config.activate_profile("openai"));
    assert_eq!(config.base_url, "https://api.openai.com/v1");
    assert_eq!(config.model, "gpt-4o-mini");
    assert_eq!(config.api_key, "legacy-openai");

    assert!(config.activate_profile("deepseek"));
    assert_eq!(config.api_key, "new-deepseek-key");

    config.save(state.path()).unwrap();
    let reloaded = AiConfig::load(state.path());
    assert_eq!(reloaded, config);
    assert_eq!(reloaded.profiles.len(), 3);
}

#[test]
fn corrupted_config_falls_back_to_defaults() {
    let state = TempDir::new("config-corrupt");
    std::fs::write(AiConfig::config_path(state.path()), "definitely not json").unwrap();
    assert_eq!(AiConfig::load(state.path()), AiConfig::default());
}

#[test]
fn environment_key_has_priority() {
    // `cargo test` runs each integration test file in its own process, so this
    // environment mutation cannot race other test binaries.
    let previous = std::env::var("RDM_AI_API_KEY").ok();
    let config = AiConfig {
        enabled: true,
        api_key: "stored-key".to_owned(),
        ..AiConfig::default()
    };

    std::env::set_var("RDM_AI_API_KEY", "env-key");
    assert_eq!(config.resolved_api_key(), "env-key");
    assert!(config.is_configured());

    std::env::set_var("RDM_AI_API_KEY", "   ");
    assert_eq!(config.resolved_api_key(), "stored-key");

    std::env::remove_var("RDM_AI_API_KEY");
    assert_eq!(config.resolved_api_key(), "stored-key");

    match previous {
        Some(value) => std::env::set_var("RDM_AI_API_KEY", value),
        None => std::env::remove_var("RDM_AI_API_KEY"),
    }
}

#[test]
fn debug_output_is_redacted() {
    let config = AiConfig {
        enabled: true,
        api_key: "must-not-leak-123".to_owned(),
        ..AiConfig::default()
    };
    let rendered = format!("{config:?}");
    assert!(!rendered.contains("must-not-leak-123"), "{rendered}");
}

//! `Recipe::check_tests` — pure, network-free self-checks attached to a recipe
//! (mirrors yt-dlp `_TESTS`; see framework §6.2).

mod common;

use std::collections::BTreeMap;

use rdm_resolve::Recipe;
use serde_json::json;

use common::recipe_json;

fn self_test_recipe(tests: serde_json::Value) -> Recipe {
    let mut value: serde_json::Value = serde_json::from_str(&recipe_json(
        "self-test-recipe",
        1,
        json!({
            "hosts": ["example.com"],
            "url_pattern": "^https://example\\.com/d/(?P<id>\\d+)$"
        }),
        json!([{
            "id": "check",
            "op": "assert",
            "args": { "kind": "equals", "value": "ok", "expected": "ok" }
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    ))
    .expect("fixture recipe JSON");
    value["tests"] = tests;
    serde_json::from_value(value).expect("self-test recipe deserializes")
}

#[test]
fn check_tests_verifies_match_no_match_and_skip() {
    let recipe = self_test_recipe(json!([
        { "name": "matches-detail", "input_url": "https://example.com/d/123" },
        { "name": "no-match-expected", "input_url": "https://example.com/about", "expect_no_match": true },
        { "name": "match-but-expected-no-match", "input_url": "https://example.com/d/1", "expect_no_match": true },
        { "name": "no-match-but-expected-match", "input_url": "https://example.com/about" },
        { "name": "wrong-host", "input_url": "https://evil.com/d/1" },
        { "name": "dead-link", "input_url": "https://example.com/d/9", "skip": "upstream 410" },
        { "name": "blank-skip-reason", "input_url": "https://example.com/d/9", "skip": "   " },
        { "name": "invalid-input", "input_url": "not a url" }
    ]));

    let results = recipe.check_tests();
    assert_eq!(results.len(), 8);
    let by_name: BTreeMap<&str, _> = results
        .iter()
        .map(|result| (result.name.as_str(), result))
        .collect();

    let matches = by_name["matches-detail"];
    assert!(matches.ok, "{}", matches.message);
    assert!(!matches.skipped);
    assert!(matches.message.contains("matched"));

    let no_match = by_name["no-match-expected"];
    assert!(
        no_match.ok,
        "expect_no_match test passes: {}",
        no_match.message
    );
    assert!(!no_match.skipped);

    let match_but_expected_no_match = by_name["match-but-expected-no-match"];
    assert!(!match_but_expected_no_match.ok);
    assert!(!match_but_expected_no_match.skipped);
    assert!(
        match_but_expected_no_match
            .message
            .contains("must not match"),
        "message: {}",
        match_but_expected_no_match.message
    );

    let no_match_but_expected_match = by_name["no-match-but-expected-match"];
    assert!(!no_match_but_expected_match.ok);
    assert!(
        no_match_but_expected_match.message.contains("must match"),
        "message: {}",
        no_match_but_expected_match.message
    );

    let wrong_host = by_name["wrong-host"];
    assert!(
        !wrong_host.ok,
        "host mismatch means the recipe does not apply"
    );

    let dead_link = by_name["dead-link"];
    assert!(dead_link.ok, "a skipped test is not a failure");
    assert!(dead_link.skipped);
    assert!(
        dead_link.message.contains("upstream 410"),
        "skip reason is preserved: {}",
        dead_link.message
    );

    let blank_skip_reason = by_name["blank-skip-reason"];
    assert!(!blank_skip_reason.ok, "an empty skip reason is untrusted");
    assert!(!blank_skip_reason.skipped);

    let invalid_input = by_name["invalid-input"];
    assert!(!invalid_input.ok);
    assert!(
        invalid_input.message.contains("invalid input_url"),
        "message: {}",
        invalid_input.message
    );
}

#[test]
fn check_tests_is_empty_when_no_tests_are_attached() {
    let recipe: Recipe = serde_json::from_str(&recipe_json(
        "no-tests",
        1,
        json!({ "hosts": ["example.com"] }),
        json!([{
            "id": "check",
            "op": "assert",
            "args": { "kind": "equals", "value": "ok", "expected": "ok" }
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    ))
    .expect("deserializes");
    assert!(recipe.check_tests().is_empty());
}

#[test]
fn check_tests_is_pure_and_never_runs_steps() {
    // The only step would need HTTP and the URL is not reachable; if
    // `check_tests` executed steps this would error or hang.
    let recipe: Recipe = serde_json::from_str(&recipe_json(
        "pure-self-check",
        1,
        json!({ "hosts": ["example.com"] }),
        json!([{
            "id": "fetch",
            "op": "http_request",
            "args": { "url": "http://127.0.0.1:9/api" },
            "assign": { "body": "body_text" }
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    ))
    .expect("deserializes");

    let results = recipe.check_tests();
    assert!(results.is_empty(), "no tests means no self-check output");
}

#[test]
fn check_tests_accepts_defaulted_test_fields() {
    // A minimal `_TESTS`-style entry: no expect_no_match, no skip, no host hint.
    let recipe = self_test_recipe(json!([
        { "name": "minimal", "input_url": "https://example.com/d/77" }
    ]));

    let results = recipe.check_tests();
    assert_eq!(results.len(), 1);
    assert!(results[0].ok);
    assert!(!results[0].skipped);
}

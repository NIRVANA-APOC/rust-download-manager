//! Recipe operator matrix (`docs/research/link-resolution-framework.md` §6.3):
//! template, assert predicates, url_join, regex_capture, json_query,
//! `when` guards, `branch`, `bounded_each` and `on_fail`.
//!
//! Operators run through the public `RecipeRunner`; no network is used unless a
//! test explicitly needs a JSON body from the loopback mock server.

mod common;
mod support;

use std::collections::BTreeMap;

use rdm_resolve::{
    Recipe, ResolveErrorKind, DEFAULT_BOUNDED_EACH_MAX, MAX_BOUNDED_EACH_DEPTH,
    MAX_BOUNDED_EACH_ITEMS, SUPPORTED_OPS,
};
use serde_json::{json, Value};

use common::{recipe_json, recipe_value, run_recipe, run_recipe_counted, seed};
use support::{MockServer, Reply};

fn ops_recipe(steps: Value, emit: Value) -> Recipe {
    recipe_value("ops-fixture", 1, json!({}), steps, emit)
}

fn ops_recipe_json(steps: Value, emit: Value) -> String {
    recipe_json("ops-fixture", 1, json!({}), steps, emit)
}

/// A one-step assert recipe that emits a fixed URL.
fn assert_recipe(arguments: Value) -> Recipe {
    ops_recipe(
        json!([{ "id": "check", "op": "assert", "args": arguments }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    )
}

#[test]
fn supported_ops_cover_the_documented_v1_operator_set() {
    let expected = [
        "http_request",
        "json_query",
        "regex_capture",
        "url_join",
        "template",
        "assert",
        "branch",
        "bounded_each",
    ];
    for op in expected {
        assert!(SUPPORTED_OPS.contains(&op), "missing operator {op:?}");
    }
    assert_eq!(SUPPORTED_OPS.len(), expected.len());
    assert_eq!(DEFAULT_BOUNDED_EACH_MAX, 20);
    assert_eq!(MAX_BOUNDED_EACH_ITEMS, 100);
    assert_eq!(MAX_BOUNDED_EACH_DEPTH, 4);
}

// ---------------------------------------------------------------------------
// template
// ---------------------------------------------------------------------------

#[tokio::test]
async fn template_renders_seeded_variables() {
    let recipe = ops_recipe(
        json!([{
            "id": "join",
            "op": "template",
            "args": { "template": "prefix-{{a}}-{{b}}" },
            "assign": { "joined": "value" }
        }]),
        json!({
            "url": "https://cdn.example.com/{{joined}}.bin",
            "name": "{{joined}}.bin"
        }),
    );

    let (output, steps) = run_recipe_counted(&recipe, seed(&[("a", "one"), ("b", "two")]))
        .await
        .expect("template renders");

    assert_eq!(output.url, "https://cdn.example.com/prefix-one-two.bin");
    assert_eq!(output.name.as_deref(), Some("prefix-one-two.bin"));
    assert_eq!(
        output.vars.get("joined").map(String::as_str),
        Some("prefix-one-two")
    );
    assert_eq!(steps, 1);
}

#[tokio::test]
async fn template_leaves_plain_text_untouched() {
    let recipe = ops_recipe(
        json!([{
            "id": "join",
            "op": "template",
            "args": { "template": "static-only" },
            "assign": { "joined": "value" }
        }]),
        json!({ "url": "https://cdn.example.com/{{joined}}.bin" }),
    );

    let output = run_recipe(&recipe, BTreeMap::new())
        .await
        .expect("template renders");
    assert_eq!(output.url, "https://cdn.example.com/static-only.bin");
}

#[tokio::test]
async fn undefined_template_variable_is_recipe_invalid() {
    let recipe = ops_recipe(
        json!([{
            "id": "join",
            "op": "template",
            "args": { "template": "{{missing}}" },
            "assign": { "joined": "value" }
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    );

    let error = run_recipe(&recipe, BTreeMap::new())
        .await
        .expect_err("undefined variable must fail loudly");

    assert_eq!(error.kind, ResolveErrorKind::RecipeInvalid);
    assert_eq!(error.step_id.as_deref(), Some("join"));
    assert!(error.detail.contains("missing"), "detail: {}", error.detail);
}

#[tokio::test]
async fn unterminated_template_placeholder_is_recipe_invalid() {
    let recipe = ops_recipe(
        json!([{
            "id": "join",
            "op": "template",
            "args": { "template": "{{a" },
            "assign": { "joined": "value" }
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    );

    let error = run_recipe(&recipe, seed(&[("a", "x")]))
        .await
        .expect_err("unterminated placeholder is invalid");
    assert_eq!(error.kind, ResolveErrorKind::RecipeInvalid);
    assert!(error.detail.contains("unterminated"));
}

// ---------------------------------------------------------------------------
// assert
// ---------------------------------------------------------------------------

#[tokio::test]
async fn assert_host_suffix_predicate() {
    let pass = assert_recipe(json!({
        "kind": "host_suffix",
        "value": "https://cdn.example.com/files/a.bin",
        "suffix": ".example.com"
    }));
    assert!(run_recipe(&pass, BTreeMap::new()).await.is_ok());

    let fail = assert_recipe(json!({
        "kind": "host_suffix",
        "value": "https://evil.example.net/files/a.bin",
        "suffix": ".example.com"
    }));
    let error = run_recipe(&fail, BTreeMap::new())
        .await
        .expect_err("wrong host must fail");
    assert_eq!(error.kind, ResolveErrorKind::ExpectFailed);
}

#[tokio::test]
async fn assert_contains_and_equals_predicates() {
    let contains_pass = assert_recipe(json!({
        "kind": "contains",
        "value": "https://cdn.example.com/files/setup-x64.exe",
        "needle": "setup-"
    }));
    assert!(run_recipe(&contains_pass, BTreeMap::new()).await.is_ok());

    let contains_fail = assert_recipe(json!({
        "kind": "contains",
        "value": "https://cdn.example.com/files/readme.txt",
        "needle": "setup-"
    }));
    assert_eq!(
        run_recipe(&contains_fail, BTreeMap::new())
            .await
            .expect_err("missing needle must fail")
            .kind,
        ResolveErrorKind::ExpectFailed
    );

    let equals_pass = assert_recipe(json!({
        "kind": "equals",
        "value": "{{status}}",
        "expected": "ready"
    }));
    assert!(run_recipe(&equals_pass, seed(&[("status", "ready")]))
        .await
        .is_ok());

    let equals_fail = assert_recipe(json!({
        "kind": "equals",
        "value": "{{status}}",
        "expected": "ready"
    }));
    assert_eq!(
        run_recipe(&equals_fail, seed(&[("status", "failed")]))
            .await
            .expect_err("mismatched values must fail")
            .kind,
        ResolveErrorKind::ExpectFailed
    );
}

#[tokio::test]
async fn assert_matches_predicate_uses_regex() {
    let pass = assert_recipe(json!({
        "kind": "matches",
        "value": "https://cdn.example.com/build/rdm-1.2.3-x64.zip",
        "pattern": "^https://cdn\\.example\\.com/build/rdm-[0-9.]+-x64\\.zip$"
    }));
    assert!(run_recipe(&pass, BTreeMap::new()).await.is_ok());

    let fail = assert_recipe(json!({
        "kind": "matches",
        "value": "https://cdn.example.com/build/rdm-1.2.3-x64.zip",
        "pattern": "^https://cdn\\.example\\.com/build/rdm-[0-9.]+-arm64\\.zip$"
    }));
    let error = run_recipe(&fail, BTreeMap::new())
        .await
        .expect_err("regex mismatch must fail");
    assert_eq!(error.kind, ResolveErrorKind::ExpectFailed);
}

#[test]
fn assert_validation_rejects_bad_patterns_and_kinds() {
    let invalid_pattern = Recipe::from_json(&ops_recipe_json(
        json!([{
            "id": "check",
            "op": "assert",
            "args": { "kind": "matches", "value": "x", "pattern": "[" }
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    ))
    .expect_err("invalid regex must be rejected at load time");
    assert!(invalid_pattern.contains("invalid assert pattern"));

    let missing_pattern = Recipe::from_json(&ops_recipe_json(
        json!([{
            "id": "check",
            "op": "assert",
            "args": { "kind": "matches", "value": "x" }
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    ))
    .expect_err("matches without pattern must be rejected");
    assert!(missing_pattern.contains("pattern"));

    let unknown_kind = Recipe::from_json(&ops_recipe_json(
        json!([{
            "id": "check",
            "op": "assert",
            "args": { "kind": "starts_with", "value": "x", "prefix": "x" }
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    ))
    .expect_err("unknown assert kind must be rejected");
    assert!(unknown_kind.contains("starts_with"));
}

// ---------------------------------------------------------------------------
// url_join / regex_capture / json_query
// ---------------------------------------------------------------------------

#[tokio::test]
async fn url_join_resolves_relative_references() {
    let recipe = ops_recipe(
        json!([{
            "id": "join",
            "op": "url_join",
            "args": {
                "base": "{{base}}",
                "relative": "../files/setup.exe"
            },
            "assign": { "joined": "value" }
        }]),
        json!({ "url": "{{joined}}" }),
    );

    let output = run_recipe(
        &recipe,
        seed(&[("base", "https://cdn.example.com/dir/page.html")]),
    )
    .await
    .expect("relative URL joins");
    assert_eq!(output.url, "https://cdn.example.com/files/setup.exe");
}

#[tokio::test]
async fn url_join_rejects_invalid_base() {
    let recipe = ops_recipe(
        json!([{
            "id": "join",
            "op": "url_join",
            "args": { "base": "not a url", "relative": "a.bin" },
            "assign": { "joined": "value" }
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    );

    let error = run_recipe(&recipe, BTreeMap::new())
        .await
        .expect_err("invalid base URL fails");
    assert_eq!(error.kind, ResolveErrorKind::InvalidUrl);
    assert_eq!(error.step_id.as_deref(), Some("join"));
}

#[tokio::test]
async fn regex_capture_extracts_a_group() {
    let recipe = ops_recipe(
        json!([{
            "id": "capture",
            "op": "regex_capture",
            "args": {
                "from": "text",
                "pattern": "version=(?P<ver>[0-9.]+)",
                "group": 1
            },
            "assign": { "version": "value" }
        }]),
        json!({ "url": "https://cdn.example.com/{{version}}.msi" }),
    );

    let output = run_recipe(&recipe, seed(&[("text", "app version=1.2.3 build=7")]))
        .await
        .expect("regex captures");
    assert_eq!(output.url, "https://cdn.example.com/1.2.3.msi");
}

#[tokio::test]
async fn regex_capture_miss_and_invalid_regex_are_typed() {
    let miss = ops_recipe(
        json!([{
            "id": "capture",
            "op": "regex_capture",
            "args": { "from": "text", "pattern": "version=([0-9.]+)" },
            "assign": { "version": "value" }
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    );
    let error = run_recipe(&miss, seed(&[("text", "no numbers here")]))
        .await
        .expect_err("missed regex must be a selector miss");
    assert_eq!(error.kind, ResolveErrorKind::SelectorMiss);

    let invalid = ops_recipe(
        json!([{
            "id": "capture",
            "op": "regex_capture",
            "args": { "from": "text", "pattern": "[" },
            "assign": { "version": "value" }
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    );
    let error = run_recipe(&invalid, seed(&[("text", "anything")]))
        .await
        .expect_err("invalid regex is a recipe error");
    assert_eq!(error.kind, ResolveErrorKind::RecipeInvalid);

    let unknown_from = ops_recipe(
        json!([{
            "id": "capture",
            "op": "regex_capture",
            "args": { "from": "missing", "pattern": "(.*)" },
            "assign": { "version": "value" }
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    );
    let error = run_recipe(&unknown_from, BTreeMap::new())
        .await
        .expect_err("unknown source variable is invalid");
    assert_eq!(error.kind, ResolveErrorKind::RecipeInvalid);
}

#[tokio::test]
async fn json_query_miss_is_selector_miss() {
    let server = MockServer::start(|_| Reply::json("{\"ok\":true}")).await;
    let recipe = ops_recipe(
        json!([
            {
                "id": "fetch",
                "op": "http_request",
                "args": { "url": "{{base}}/api" },
                "assign": { "body": "body_json" }
            },
            {
                "id": "pointer",
                "op": "json_query",
                "args": { "from": "body", "pointer": "/missing" },
                "assign": { "value_of_interest": "value" }
            }
        ]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    );

    let error = run_recipe(&recipe, seed(&[("base", server.base_url())]))
        .await
        .expect_err("missing pointer must be a selector miss");

    assert_eq!(error.kind, ResolveErrorKind::SelectorMiss);
    assert_eq!(error.step_id.as_deref(), Some("pointer"));
    assert_eq!(server.count_method("GET"), 1);
}

// ---------------------------------------------------------------------------
// when guards
// ---------------------------------------------------------------------------

#[tokio::test]
async fn when_guard_skips_a_step_without_failing() {
    let recipe = ops_recipe(
        json!([
            {
                "id": "guarded",
                "op": "assert",
                "args": { "kind": "equals", "value": "a", "expected": "b" },
                "when": { "kind": "equals", "var": "flag", "expected": "yes" }
            },
            {
                "id": "always",
                "op": "assert",
                "args": { "kind": "equals", "value": "ok", "expected": "ok" }
            }
        ]),
        json!({ "url": "https://cdn.example.com/{{flag}}.bin" }),
    );

    let (output, steps) = run_recipe_counted(&recipe, seed(&[("flag", "no")]))
        .await
        .expect("a skipped step is not a failure");
    assert_eq!(steps, 1, "only the unguarded step ran");
    assert_eq!(output.url, "https://cdn.example.com/no.bin");

    let error = run_recipe(&recipe, seed(&[("flag", "yes")]))
        .await
        .expect_err("guard passed, so the failing assertion runs");
    assert_eq!(error.kind, ResolveErrorKind::ExpectFailed);
    assert_eq!(error.step_id.as_deref(), Some("guarded"));
}

#[tokio::test]
async fn when_exists_guard_depends_only_on_presence() {
    let recipe = ops_recipe(
        json!([{
            "id": "guarded",
            "op": "assert",
            "args": { "kind": "equals", "value": "ok", "expected": "ok" },
            "when": { "kind": "exists", "var": "optional" }
        }]),
        json!({ "url": "https://cdn.example.com/{{flag}}.bin" }),
    );

    let (_, steps) = run_recipe_counted(&recipe, seed(&[("flag", "a"), ("optional", "x")]))
        .await
        .expect("guard passes when the variable exists");
    assert_eq!(steps, 1);

    let (_, steps) = run_recipe_counted(&recipe, seed(&[("flag", "a")]))
        .await
        .expect("guard fails when the variable is missing");
    assert_eq!(steps, 0);
}

#[tokio::test]
async fn skipped_when_guard_prevents_network_side_effects() {
    let server = MockServer::start(|_| Reply::json("{\"ok\":true}")).await;
    let recipe = ops_recipe(
        json!([{
            "id": "fetch",
            "op": "http_request",
            "args": { "url": "{{base}}/api" },
            "when": { "kind": "equals", "var": "flag", "expected": "yes" },
            "assign": { "body": "body_text" }
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    );

    let output = run_recipe(
        &recipe,
        seed(&[("base", server.base_url()), ("flag", "no")]),
    )
    .await
    .expect("skipped HTTP step is not a failure");

    assert_eq!(output.url, "https://cdn.example.com/file.bin");
    assert_eq!(
        server.request_count(),
        0,
        "a false `when` guard must not issue a request"
    );
}

#[test]
fn when_guard_validation_rejects_bad_conditions() {
    let unknown_kind = Recipe::from_json(&ops_recipe_json(
        json!([{
            "id": "s",
            "op": "assert",
            "args": { "kind": "equals", "value": "a", "expected": "a" },
            "when": { "kind": "present", "var": "flag" }
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    ))
    .expect_err("unknown when kind must be rejected");
    assert!(unknown_kind.contains("present"));

    let missing_expected = Recipe::from_json(&ops_recipe_json(
        json!([{
            "id": "s",
            "op": "assert",
            "args": { "kind": "equals", "value": "a", "expected": "a" },
            "when": { "kind": "equals", "var": "flag" }
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    ))
    .expect_err("equals without expected must be rejected");
    assert!(missing_expected.contains("expected"));

    let empty_var = Recipe::from_json(&ops_recipe_json(
        json!([{
            "id": "s",
            "op": "assert",
            "args": { "kind": "equals", "value": "a", "expected": "a" },
            "when": { "kind": "exists", "var": "" }
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    ))
    .expect_err("empty var must be rejected");
    assert!(empty_var.contains("var"));
}

// ---------------------------------------------------------------------------
// branch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn branch_sets_a_boolean_flag_for_later_guards() {
    let recipe = ops_recipe(
        json!([
            {
                "id": "branch",
                "op": "branch",
                "args": { "kind": "equals", "var": "flag", "expected": "yes" },
                "assign": { "is_yes": "value" }
            },
            {
                "id": "guarded",
                "op": "assert",
                "args": { "kind": "equals", "value": "ok", "expected": "ok" },
                "when": { "kind": "equals", "var": "is_yes", "expected": "true" }
            }
        ]),
        json!({ "url": "https://cdn.example.com/{{is_yes}}.bin" }),
    );

    let (output, steps) = run_recipe_counted(&recipe, seed(&[("flag", "yes")]))
        .await
        .expect("true branch");
    assert_eq!(output.url, "https://cdn.example.com/true.bin");
    assert_eq!(steps, 2, "branch and the guarded step both ran");

    let (output, steps) = run_recipe_counted(&recipe, seed(&[("flag", "no")]))
        .await
        .expect("false branch");
    assert_eq!(output.url, "https://cdn.example.com/false.bin");
    assert_eq!(steps, 1, "only the branch step ran");
}

#[tokio::test]
async fn branch_exists_condition_reports_presence() {
    let recipe = ops_recipe(
        json!([{
            "id": "branch",
            "op": "branch",
            "args": { "kind": "exists", "var": "optional" },
            "assign": { "present": "value" }
        }]),
        json!({ "url": "https://cdn.example.com/{{present}}.bin" }),
    );

    let output = run_recipe(&recipe, seed(&[("optional", "x")]))
        .await
        .expect("present variable");
    assert_eq!(output.url, "https://cdn.example.com/true.bin");

    let output = run_recipe(&recipe, BTreeMap::new())
        .await
        .expect("absent variable");
    assert_eq!(output.url, "https://cdn.example.com/false.bin");
}

// ---------------------------------------------------------------------------
// bounded_each
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bounded_each_iterates_json_arrays_in_order() {
    let server = MockServer::start(|_| {
        Reply::json("{\"items\":[{\"name\":\"a\"},{\"name\":\"b\"},{\"name\":\"c\"}]}")
    })
    .await;
    let recipe = ops_recipe(
        json!([
            {
                "id": "fetch",
                "op": "http_request",
                "args": { "url": "{{base}}/api" },
                "assign": { "body": "body_json" }
            },
            {
                "id": "items",
                "op": "json_query",
                "args": { "from": "body", "pointer": "/items" },
                "assign": { "items": "value" }
            },
            {
                "id": "each",
                "op": "bounded_each",
                "args": {
                    "from": "items",
                    "as": "item",
                    "max": 5,
                    "steps": [
                        {
                            "id": "name",
                            "op": "json_query",
                            "args": { "from": "item", "pointer": "/name" },
                            "assign": { "name": "value" }
                        },
                        {
                            "id": "append",
                            "op": "template",
                            "args": { "template": "{{acc}}{{name}}" },
                            "assign": { "acc": "value" }
                        }
                    ]
                }
            }
        ]),
        json!({ "url": "https://cdn.example.com/{{acc}}.bin" }),
    );

    let (output, steps) =
        run_recipe_counted(&recipe, seed(&[("base", server.base_url()), ("acc", "")]))
            .await
            .expect("bounded_each iterates");

    assert_eq!(output.url, "https://cdn.example.com/abc.bin");
    assert_eq!(output.vars.get("acc").map(String::as_str), Some("abc"));
    assert!(steps >= 3, "outer steps are counted: {steps}");
}

#[tokio::test]
async fn bounded_each_over_limit_is_recipe_budget_exceeded() {
    let items: Vec<u8> = (0..30).collect();
    let body = json!({ "items": items }).to_string();
    let server = MockServer::start(move |_| Reply::json(&body)).await;
    let recipe = ops_recipe(
        json!([
            {
                "id": "fetch",
                "op": "http_request",
                "args": { "url": "{{base}}/api" },
                "assign": { "body": "body_json" }
            },
            {
                "id": "items",
                "op": "json_query",
                "args": { "from": "body", "pointer": "/items" },
                "assign": { "items": "value" }
            },
            {
                "id": "each",
                "op": "bounded_each",
                "args": {
                    "from": "items",
                    "max": 20,
                    "steps": [{
                        "id": "render",
                        "op": "template",
                        "args": { "template": "item={{item}}" },
                        "assign": { "last": "value" }
                    }]
                }
            }
        ]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    );

    let error = run_recipe(&recipe, seed(&[("base", server.base_url())]))
        .await
        .expect_err("item count above max must fail");

    assert_eq!(error.kind, ResolveErrorKind::RecipeBudgetExceeded);
    assert_eq!(error.step_id.as_deref(), Some("each"));
    assert!(
        error.detail.contains("exceeds max"),
        "detail: {}",
        error.detail
    );
}

#[tokio::test]
async fn bounded_each_requires_a_json_array_source() {
    let recipe = ops_recipe(
        json!([{
            "id": "each",
            "op": "bounded_each",
            "args": {
                "from": "text",
                "steps": [{
                    "id": "render",
                    "op": "template",
                    "args": { "template": "{{item}}" },
                    "assign": { "last": "value" }
                }]
            }
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    );

    let error = run_recipe(&recipe, seed(&[("text", "not an array")]))
        .await
        .expect_err("non-array source must fail");
    assert_eq!(error.kind, ResolveErrorKind::RecipeInvalid);
    assert!(error.detail.contains("not a JSON array"));

    let error = run_recipe(&recipe, BTreeMap::new())
        .await
        .expect_err("unknown source variable must fail");
    assert_eq!(error.kind, ResolveErrorKind::RecipeInvalid);
    assert!(error.detail.contains("unknown variable"));
}

#[test]
fn bounded_each_validation_checks_max_depth_ids_and_operators() {
    let emit = json!({ "url": "https://cdn.example.com/file.bin" });

    for bad_max in [json!(0), json!(101), json!("ten")] {
        let recipe = ops_recipe_json(
            json!([{
                "id": "each",
                "op": "bounded_each",
                "args": {
                    "from": "items",
                    "max": bad_max,
                    "steps": [{
                        "id": "render",
                        "op": "template",
                        "args": { "template": "x" },
                        "assign": { "last": "value" }
                    }]
                }
            }]),
            emit.clone(),
        );
        assert!(
            Recipe::from_json(&recipe).is_err(),
            "max {bad_max} must fail"
        );
    }

    // Nesting beyond the documented depth limit is rejected at load time.
    let too_deep = ops_recipe_json(
        nested_bounded_each(MAX_BOUNDED_EACH_DEPTH + 1),
        emit.clone(),
    );
    let error = Recipe::from_json(&too_deep).expect_err("too-deep nesting must be rejected");
    assert!(error.contains("nesting"), "error: {error}");

    // ... while the documented maximum depth is accepted.
    let allowed = ops_recipe_json(nested_bounded_each(MAX_BOUNDED_EACH_DEPTH), emit.clone());
    assert!(Recipe::from_json(&allowed).is_ok());

    let duplicate_id = ops_recipe_json(
        json!([{
            "id": "dup",
            "op": "bounded_each",
            "args": {
                "from": "items",
                "steps": [{
                    "id": "dup",
                    "op": "assert",
                    "args": { "kind": "equals", "value": "a", "expected": "a" }
                }]
            }
        }]),
        emit.clone(),
    );
    let error = Recipe::from_json(&duplicate_id).expect_err("nested duplicate ids must fail");
    assert!(error.contains("duplicate"), "error: {error}");

    let bad_nested_op = ops_recipe_json(
        json!([{
            "id": "each",
            "op": "bounded_each",
            "args": {
                "from": "items",
                "steps": [{ "id": "nested", "op": "eval_js" }]
            }
        }]),
        emit.clone(),
    );
    assert!(Recipe::from_json(&bad_nested_op).is_err());

    let empty_nested = ops_recipe_json(
        json!([{
            "id": "each",
            "op": "bounded_each",
            "args": { "from": "items", "steps": [] }
        }]),
        emit,
    );
    assert!(Recipe::from_json(&empty_nested).is_err());
}

fn nested_bounded_each(depth: u32) -> Value {
    let mut steps = json!([{
        "id": "leaf",
        "op": "assert",
        "args": { "kind": "equals", "value": "ok", "expected": "ok" }
    }]);
    for level in 0..depth {
        steps = json!([{
            "id": format!("each-{level}"),
            "op": "bounded_each",
            "args": { "from": "items", "steps": steps }
        }]);
    }
    steps
}

// ---------------------------------------------------------------------------
// on_fail
// ---------------------------------------------------------------------------

#[tokio::test]
async fn on_fail_overrides_the_reported_error_kind() {
    let recipe = ops_recipe(
        json!([{
            "id": "check",
            "op": "assert",
            "args": { "kind": "equals", "value": "a", "expected": "b" },
            "on_fail": "recipe_invalid"
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    );

    let error = run_recipe(&recipe, BTreeMap::new())
        .await
        .expect_err("assertion fails");
    assert_eq!(error.kind, ResolveErrorKind::RecipeInvalid);
    assert_eq!(error.step_id.as_deref(), Some("check"));
}

#[tokio::test]
async fn assert_failure_defaults_to_expect_failed() {
    let recipe = assert_recipe(json!({
        "kind": "equals",
        "value": "a",
        "expected": "b"
    }));

    let error = run_recipe(&recipe, BTreeMap::new())
        .await
        .expect_err("assertion fails");
    assert_eq!(error.kind, ResolveErrorKind::ExpectFailed);
}

#[tokio::test]
async fn on_fail_accepts_pascal_case_aliases() {
    let recipe = ops_recipe(
        json!([{
            "id": "check",
            "op": "assert",
            "args": { "kind": "equals", "value": "a", "expected": "b" },
            "on_fail": "RecipeStale"
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    );

    let error = run_recipe(&recipe, BTreeMap::new())
        .await
        .expect_err("assertion fails");
    assert_eq!(error.kind, ResolveErrorKind::RecipeStale);
}

#[test]
fn invalid_on_fail_name_is_rejected_at_load_time() {
    let rejected = Recipe::from_json(&ops_recipe_json(
        json!([{
            "id": "check",
            "op": "assert",
            "args": { "kind": "equals", "value": "a", "expected": "a" },
            "on_fail": "made_up_kind"
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    ))
    .expect_err("invalid on_fail must be rejected");
    assert!(rejected.contains("made_up_kind"), "error: {rejected}");

    let accepted = Recipe::from_json(&ops_recipe_json(
        json!([{
            "id": "check",
            "op": "assert",
            "args": { "kind": "equals", "value": "a", "expected": "a" },
            "on_fail": "recipe_stale"
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    ));
    assert!(
        accepted.is_ok(),
        "snake_case kind names are valid on_fail values"
    );
}

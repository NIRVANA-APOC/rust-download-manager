//! Declarative, model-free resolution recipes.
//!
//! A recipe is a bounded pipeline of typed operators. It never calls a model:
//! replay is deterministic, so a recipe that passes its tests today must behave
//! the same tomorrow (or fail loudly).

use std::collections::BTreeMap;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::contract::{now_ms, Budget, ResolveErrorKind};
use crate::http::{HttpRequestSpec, HttpTool, Observation};

/// Recipe schema identifier.
pub const RECIPE_SCHEMA: &str = "rdm.recipe/v1";
/// Operator names accepted by [`Recipe::validate`].
pub const SUPPORTED_OPS: &[&str] = &[
    "http_request",
    "json_query",
    "regex_capture",
    "url_join",
    "template",
    "assert",
    "branch",
    "bounded_each",
];
/// Maximum number of nested `bounded_each` levels (`docs/architecture.md` §11.4).
pub const MAX_BOUNDED_EACH_DEPTH: u32 = 4;
/// Default hard cap on `bounded_each` iterations.
pub const DEFAULT_BOUNDED_EACH_MAX: u32 = 20;
/// Largest accepted `bounded_each.max` (validated at load time).
pub const MAX_BOUNDED_EACH_ITEMS: u32 = 100;

/// One reusable resolution method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recipe {
    pub schema: String,
    pub id: String,
    pub version: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "match")]
    pub matcher: RecipeMatch,
    #[serde(default)]
    pub captures: Vec<String>,
    #[serde(default)]
    pub limits: RecipeLimits,
    pub steps: Vec<RecipeStep>,
    pub emit: RecipeEmit,
    #[serde(default)]
    pub tests: Vec<RecipeTest>,
    /// Reusability diagnostics written by [`crate::generalize`] (design 18
    /// §2.1). Additive and optional: older files simply have no value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generalization: Option<RecipeGeneralization>,
}

/// How a recipe was abstracted into a reusable template (design 18 §2.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecipeGeneralization {
    /// `www.bilibili.com/video` (UI display / same-class grouping key).
    pub scope: String,
    /// Abstracted variable names (mirrors [`Recipe::captures`]).
    pub captures: Vec<String>,
    /// Internal parameters still hardcoded in the steps (short names such as
    /// `cid`; never URLs or credentials).
    pub leftover_hardcoded: Vec<String>,
    /// `true` = at least one variable was abstracted, so the recipe replays on
    /// same-class URLs; `false` = exact path only.
    pub reusable: bool,
}

/// Applicability: host list plus an optional URL regex.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RecipeMatch {
    pub hosts: Vec<String>,
    pub url_pattern: Option<String>,
    pub priority: i32,
}

/// Per-recipe resource limits (also clamped by the run budget).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecipeLimits {
    pub requests: u32,
    pub bytes_read: u64,
    pub wall_ms: u64,
    pub redirects: u32,
}

impl Default for RecipeLimits {
    fn default() -> Self {
        Self {
            requests: 8,
            bytes_read: 1024 * 1024,
            wall_ms: 15_000,
            redirects: 5,
        }
    }
}

impl RecipeLimits {
    pub fn to_budget(self) -> Budget {
        Budget {
            wall_ms: self.wall_ms,
            http_requests: self.requests,
            bytes_read: self.bytes_read,
            redirects: self.redirects,
        }
    }
}

/// One operator invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeStep {
    pub id: String,
    pub op: String,
    #[serde(default)]
    pub args: Value,
    #[serde(default)]
    pub expect: Option<StepExpect>,
    /// `var_name -> source`; sources are `body_json`, `body_text`, `status`,
    /// `final_url`, or `header:<name>`.
    #[serde(default)]
    pub assign: BTreeMap<String, String>,
    #[serde(default)]
    pub on_fail: Option<String>,
    /// Optional guard: when it evaluates to `false` the step is skipped instead
    /// of executed (a skip is not a failure).
    #[serde(default)]
    pub when: Option<StepWhen>,
}

/// Per-step guard shared by `when` and `branch`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StepWhen {
    /// `exists`, `equals`, or `contains`.
    pub kind: String,
    /// Variable read by the condition.
    pub var: String,
    /// Required for `equals`/`contains`; may contain `{{var}}` templates.
    pub expected: Option<String>,
}

/// Assertions applied to the operator's observation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StepExpect {
    pub status_in: Vec<u16>,
    pub content_type_prefix: Vec<String>,
}

/// The only way a recipe reports a downloadable resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeEmit {
    pub url: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub one_time: bool,
    #[serde(default)]
    pub single_stream: bool,
    #[serde(default)]
    pub expires_in_secs: Option<u64>,
    #[serde(default)]
    pub expected_sha256: Option<String>,
}

/// Self-test attached to a recipe (mirrors yt-dlp `_TESTS`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeTest {
    pub name: String,
    pub input_url: String,
    #[serde(default)]
    pub expect_host_suffix: Option<String>,
    #[serde(default)]
    pub expect_no_match: bool,
    /// Tests that cannot run (dead link, needs login) must say why.
    #[serde(default)]
    pub skip: Option<String>,
}

/// Outcome of one [`RecipeTest`] pure matching check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeTestResult {
    pub name: String,
    /// `true` when the test passed (or was skipped with a reason).
    pub ok: bool,
    /// `true` when the test declared a `skip` reason and was not executed.
    pub skipped: bool,
    pub message: String,
}

impl Recipe {
    /// Parse and validate a recipe from JSON.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let recipe: Recipe =
            serde_json::from_str(text).map_err(|error| format!("invalid recipe JSON: {error}"))?;
        recipe.validate()?;
        Ok(recipe)
    }

    /// Structural validation: schema, ids, operators, emit.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != RECIPE_SCHEMA {
            return Err(format!(
                "unsupported recipe schema {:?} (expected {RECIPE_SCHEMA})",
                self.schema
            ));
        }
        if self.id.trim().is_empty() {
            return Err("recipe id must not be empty".to_owned());
        }
        if self.version == 0 {
            return Err("recipe version must be >= 1".to_owned());
        }
        if self.steps.is_empty() {
            return Err("recipe must declare at least one step".to_owned());
        }
        self.validate_steps(&self.steps, 0, &mut std::collections::BTreeSet::new())?;
        if self.emit.url.trim().is_empty() {
            return Err("recipe emit.url must not be empty".to_owned());
        }
        if let Some(pattern) = &self.matcher.url_pattern {
            Regex::new(pattern).map_err(|error| format!("invalid url_pattern: {error}"))?;
        }
        Ok(())
    }

    /// Validate one step list, recursing into `bounded_each` nested steps.
    ///
    /// `depth` is the number of enclosing `bounded_each` operators.
    fn validate_steps(
        &self,
        steps: &[RecipeStep],
        depth: u32,
        seen: &mut std::collections::BTreeSet<String>,
    ) -> Result<(), String> {
        for step in steps {
            if step.id.trim().is_empty() {
                return Err("step id must not be empty".to_owned());
            }
            if !seen.insert(step.id.clone()) {
                return Err(format!("duplicate step id {:?}", step.id));
            }
            if !SUPPORTED_OPS.contains(&step.op.as_str()) {
                return Err(format!(
                    "unsupported operator {:?} in step {:?}",
                    step.op, step.id
                ));
            }
            if let Some(name) = &step.on_fail {
                if ResolveErrorKind::from_name(name).is_none() {
                    return Err(format!(
                        "step {:?}: invalid on_fail {name:?} (expected a ResolveErrorKind name such as \"recipe_invalid\")",
                        step.id
                    ));
                }
            }
            if let Some(guard) = &step.when {
                validate_condition(&guard.kind, &guard.var, guard.expected.as_deref())
                    .map_err(|error| format!("step {:?}: when {error}", step.id))?;
            }
            match step.op.as_str() {
                "template" => {
                    if step.args.get("template").and_then(Value::as_str).is_none() {
                        return Err(format!(
                            "step {:?}: template requires a string `template` argument",
                            step.id
                        ));
                    }
                    require_single_assign(step)?;
                }
                "branch" => {
                    let kind = step
                        .args
                        .get("kind")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            format!(
                                "step {:?}: branch requires a string `kind` argument",
                                step.id
                            )
                        })?;
                    let var = step
                        .args
                        .get("var")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            format!(
                                "step {:?}: branch requires a string `var` argument",
                                step.id
                            )
                        })?;
                    let expected = step.args.get("expected").and_then(Value::as_str);
                    validate_condition(kind, var, expected)
                        .map_err(|error| format!("step {:?}: branch {error}", step.id))?;
                    require_single_assign(step)?;
                }
                "assert" => {
                    let kind = step
                        .args
                        .get("kind")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            format!(
                                "step {:?}: assert requires a string `kind` argument",
                                step.id
                            )
                        })?;
                    match kind {
                        "host_suffix" | "contains" | "equals" => {}
                        "matches" => {
                            let pattern = assert_pattern(&step.args, &step.id)?;
                            Regex::new(pattern).map_err(|error| {
                                format!("step {:?}: invalid assert pattern: {error}", step.id)
                            })?;
                        }
                        other => {
                            return Err(format!(
                                "step {:?}: unsupported assert kind {other:?}",
                                step.id
                            ))
                        }
                    }
                }
                "bounded_each" => {
                    if depth >= MAX_BOUNDED_EACH_DEPTH {
                        return Err(format!(
                            "step {:?}: bounded_each nesting exceeds {MAX_BOUNDED_EACH_DEPTH} levels",
                            step.id
                        ));
                    }
                    if step.args.get("from").and_then(Value::as_str).is_none() {
                        return Err(format!(
                            "step {:?}: bounded_each requires a string `from` argument",
                            step.id
                        ));
                    }
                    if let Some(value) = step.args.get("max") {
                        let max = value.as_u64().ok_or_else(|| {
                            format!("step {:?}: bounded_each `max` must be an integer", step.id)
                        })?;
                        if !(1..=u64::from(MAX_BOUNDED_EACH_ITEMS)).contains(&max) {
                            return Err(format!(
                                "step {:?}: bounded_each `max` must be in 1..={MAX_BOUNDED_EACH_ITEMS}",
                                step.id
                            ));
                        }
                    }
                    if let Some(value) = step.args.get("as") {
                        let name = value.as_str().ok_or_else(|| {
                            format!("step {:?}: bounded_each `as` must be a string", step.id)
                        })?;
                        if name.trim().is_empty() {
                            return Err(format!(
                                "step {:?}: bounded_each `as` must not be empty",
                                step.id
                            ));
                        }
                    }
                    let nested = parse_nested_steps(&step.args, &step.id)?;
                    self.validate_steps(&nested, depth + 1, seen)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Run every attached [`RecipeTest`] as a pure applicability check.
    ///
    /// This never performs network I/O: `expect_no_match` tests must not match,
    /// all other tests must match, and `skip` tests are reported with their
    /// reason instead of being executed.
    pub fn check_tests(&self) -> Vec<RecipeTestResult> {
        self.tests
            .iter()
            .map(|test| self.check_test(test))
            .collect()
    }

    fn check_test(&self, test: &RecipeTest) -> RecipeTestResult {
        let name = test.name.clone();
        if let Some(reason) = &test.skip {
            if reason.trim().is_empty() {
                return RecipeTestResult {
                    name,
                    ok: false,
                    skipped: false,
                    message: "skip reason must not be empty".to_owned(),
                };
            }
            return RecipeTestResult {
                name,
                ok: true,
                skipped: true,
                message: format!("skipped: {reason}"),
            };
        }
        let url = match Url::parse(&test.input_url) {
            Ok(url) => url,
            Err(error) => {
                return RecipeTestResult {
                    name,
                    ok: false,
                    skipped: false,
                    message: format!("invalid input_url: {error}"),
                }
            }
        };
        let matched = self.matches(&url).is_some();
        let (ok, message) = if test.expect_no_match {
            if matched {
                (false, "URL must not match but did".to_owned())
            } else {
                (true, "URL did not match (as expected)".to_owned())
            }
        } else if matched {
            (true, "URL matched".to_owned())
        } else {
            (false, "URL must match but did not".to_owned())
        };
        RecipeTestResult {
            name,
            ok,
            skipped: false,
            message,
        }
    }

    /// Whether this recipe applies to `url`; returns capture variables.
    pub fn matches(&self, url: &Url) -> Option<BTreeMap<String, String>> {
        if !self.matcher.hosts.is_empty() {
            let host = url.host_str()?.to_ascii_lowercase();
            let allowed = self.matcher.hosts.iter().any(|pattern| {
                let pattern = pattern.trim().to_ascii_lowercase();
                if let Some(suffix) = pattern.strip_prefix("*.") {
                    host == suffix || host.ends_with(&format!(".{suffix}"))
                } else {
                    host == pattern
                }
            });
            if !allowed {
                return None;
            }
        }
        let Some(pattern) = &self.matcher.url_pattern else {
            return Some(BTreeMap::new());
        };
        let regex = Regex::new(pattern).ok()?;
        let captures = regex.captures(url.as_str())?;
        let mut vars = BTreeMap::new();
        for name in regex.capture_names().flatten() {
            if let Some(value) = captures.name(name) {
                vars.insert(name.to_owned(), value.as_str().to_owned());
            }
        }
        for (index, value) in self.captures.iter().enumerate() {
            if let Some(matched) = captures.get(index + 1) {
                vars.entry(value.clone())
                    .or_insert_with(|| matched.as_str().to_owned());
            }
        }
        Some(vars)
    }

    /// Applicability specificity used to break priority ties.
    pub fn specificity(&self) -> usize {
        self.matcher
            .url_pattern
            .as_ref()
            .map(|pattern| pattern.chars().filter(|ch| ch.is_alphanumeric()).count())
            .unwrap_or(0)
    }

    /// Budget this recipe needs, clamped by the caller's budget.
    pub fn budget_within(&self, outer: Budget) -> Budget {
        self.limits.to_budget().capped_by(outer)
    }
}

/// A typed template variable.
#[derive(Debug, Clone, PartialEq)]
pub enum Var {
    Text(String),
    Json(Value),
}

impl Var {
    pub fn as_text(&self) -> String {
        match self {
            Var::Text(text) => text.clone(),
            Var::Json(value) => match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            },
        }
    }
}

/// Successful recipe output before verification.
#[derive(Debug, Clone, PartialEq)]
pub struct RecipeOutput {
    pub url: String,
    pub name: Option<String>,
    pub kind: Option<String>,
    pub one_time: bool,
    pub single_stream: bool,
    pub expires_ms: Option<u64>,
    pub expected_sha256: Option<String>,
    pub vars: BTreeMap<String, String>,
}

/// Why a recipe run failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeError {
    pub kind: ResolveErrorKind,
    pub step_id: Option<String>,
    pub detail: String,
}

impl RecipeError {
    fn new(kind: ResolveErrorKind, step_id: Option<&str>, detail: impl Into<String>) -> Self {
        Self {
            kind,
            step_id: step_id.map(str::to_owned),
            detail: detail.into(),
        }
    }
}

/// Executes one recipe against one HTTP tool.
pub struct RecipeRunner<'a> {
    http: &'a mut HttpTool,
    recipe: &'a Recipe,
    vars: BTreeMap<String, Var>,
    steps_run: u32,
}

impl<'a> RecipeRunner<'a> {
    pub fn new(http: &'a mut HttpTool, recipe: &'a Recipe) -> Self {
        Self {
            http,
            recipe,
            vars: BTreeMap::new(),
            steps_run: 0,
        }
    }

    /// Seed variables captured by the applicability regex.
    pub fn seed(&mut self, vars: BTreeMap<String, String>) {
        for (name, value) in vars {
            self.vars.insert(name, Var::Text(value));
        }
    }

    pub fn steps_run(&self) -> u32 {
        self.steps_run
    }

    /// Run all steps and resolve the emit block.
    pub async fn run(&mut self) -> Result<RecipeOutput, RecipeError> {
        for step in &self.recipe.steps {
            self.execute_step(step, 0).await?;
        }
        let url = self.render(&self.recipe.emit.url, None)?;
        let name = match &self.recipe.emit.name {
            Some(template) => Some(self.render(template, None)?),
            None => None,
        };
        let expires_ms = self
            .recipe
            .emit
            .expires_in_secs
            .map(|secs| now_ms().saturating_add(secs.saturating_mul(1000)));
        let vars = self
            .vars
            .iter()
            .filter_map(|(name, value)| match value {
                Var::Text(text) => Some((name.clone(), text.clone())),
                Var::Json(_) => None,
            })
            .collect();
        Ok(RecipeOutput {
            url,
            name,
            kind: self.recipe.emit.kind.clone(),
            one_time: self.recipe.emit.one_time,
            single_stream: self.recipe.emit.single_stream,
            expires_ms,
            expected_sha256: self.recipe.emit.expected_sha256.clone(),
            vars,
        })
    }

    /// Execute one step, honouring its `when` guard and `on_fail` override.
    ///
    /// Returns `true` when the step actually ran: a skipped step is not a
    /// failure and is not counted in [`Self::steps_run`].
    async fn execute_step(&mut self, step: &RecipeStep, depth: u32) -> Result<bool, RecipeError> {
        if let Some(guard) = &step.when {
            let truth = self.eval_condition(
                &guard.kind,
                &guard.var,
                guard.expected.as_deref(),
                Some(step.id.as_str()),
            )?;
            if !truth {
                return Ok(false);
            }
        }
        match self.dispatch(step, depth).await {
            Ok(()) => {
                self.steps_run += 1;
                Ok(true)
            }
            Err(mut error) => {
                if let Some(name) = &step.on_fail {
                    if let Some(kind) = ResolveErrorKind::from_name(name) {
                        error.kind = kind;
                    }
                }
                Err(error)
            }
        }
    }

    async fn dispatch(&mut self, step: &RecipeStep, depth: u32) -> Result<(), RecipeError> {
        let step_id = Some(step.id.as_str());
        match step.op.as_str() {
            "http_request" => self.op_http_request(step, step_id).await,
            "json_query" => self.op_json_query(step, step_id),
            "regex_capture" => self.op_regex_capture(step, step_id),
            "url_join" => self.op_url_join(step, step_id),
            "template" => self.op_template(step, step_id),
            "assert" => self.op_assert(step, step_id),
            "branch" => self.op_branch(step, step_id),
            "bounded_each" => self.op_bounded_each(step, step_id, depth).await,
            other => Err(RecipeError::new(
                ResolveErrorKind::RecipeInvalid,
                step_id,
                format!("unsupported operator {other:?}"),
            )),
        }
    }

    /// Evaluate one `exists|equals|contains` condition.
    fn eval_condition(
        &self,
        kind: &str,
        var: &str,
        expected: Option<&str>,
        step_id: Option<&str>,
    ) -> Result<bool, RecipeError> {
        match kind {
            "exists" => Ok(self.vars.contains_key(var)),
            "equals" | "contains" => {
                let Some(expected) = expected else {
                    return Err(RecipeError::new(
                        ResolveErrorKind::RecipeInvalid,
                        step_id,
                        format!("condition kind {kind:?} requires `expected`"),
                    ));
                };
                let expected = self.render(expected, step_id)?;
                let Some(value) = self.vars.get(var) else {
                    return Ok(false);
                };
                let text = value.as_text();
                Ok(if kind == "equals" {
                    text == expected
                } else {
                    text.contains(&expected)
                })
            }
            other => Err(RecipeError::new(
                ResolveErrorKind::RecipeInvalid,
                step_id,
                format!("unsupported condition kind {other:?}"),
            )),
        }
    }

    fn op_template(&mut self, step: &RecipeStep, step_id: Option<&str>) -> Result<(), RecipeError> {
        let template = arg_str(&step.args, "template")?;
        let rendered = self.render(&template, step_id)?;
        let name = arg_assign_name(step)?;
        self.vars.insert(name, Var::Text(rendered));
        Ok(())
    }

    fn op_branch(&mut self, step: &RecipeStep, step_id: Option<&str>) -> Result<(), RecipeError> {
        let kind = arg_str(&step.args, "kind")?;
        let var = arg_str(&step.args, "var")?;
        let expected = arg_str_optional(&step.args, "expected");
        let truth = self.eval_condition(&kind, &var, expected.as_deref(), step_id)?;
        let name = arg_assign_name(step)?;
        self.vars.insert(
            name,
            Var::Text(if truth { "true" } else { "false" }.to_owned()),
        );
        Ok(())
    }

    async fn op_bounded_each(
        &mut self,
        step: &RecipeStep,
        step_id: Option<&str>,
        depth: u32,
    ) -> Result<(), RecipeError> {
        if depth >= MAX_BOUNDED_EACH_DEPTH {
            return Err(RecipeError::new(
                ResolveErrorKind::RecipeInvalid,
                step_id,
                format!("bounded_each nesting exceeds {MAX_BOUNDED_EACH_DEPTH} levels"),
            ));
        }
        let from = arg_str(&step.args, "from")?;
        let as_name = arg_str_optional(&step.args, "as").unwrap_or_else(|| "item".to_owned());
        let max = match step.args.get("max") {
            None | Some(Value::Null) => DEFAULT_BOUNDED_EACH_MAX,
            Some(value) => {
                let raw = value.as_u64().ok_or_else(|| {
                    RecipeError::new(
                        ResolveErrorKind::RecipeInvalid,
                        step_id,
                        "bounded_each `max` must be an integer",
                    )
                })?;
                if !(1..=u64::from(MAX_BOUNDED_EACH_ITEMS)).contains(&raw) {
                    return Err(RecipeError::new(
                        ResolveErrorKind::RecipeInvalid,
                        step_id,
                        format!("bounded_each `max` must be in 1..={MAX_BOUNDED_EACH_ITEMS}"),
                    ));
                }
                raw as u32
            }
        };
        let nested = parse_nested_steps(&step.args, &step.id)
            .map_err(|error| RecipeError::new(ResolveErrorKind::RecipeInvalid, step_id, error))?;
        let items = match self.vars.get(&from) {
            Some(Var::Json(Value::Array(items))) => items.clone(),
            Some(_) => {
                return Err(RecipeError::new(
                    ResolveErrorKind::RecipeInvalid,
                    step_id,
                    format!("step {:?}: {from:?} is not a JSON array", step.id),
                ))
            }
            None => {
                return Err(RecipeError::new(
                    ResolveErrorKind::RecipeInvalid,
                    step_id,
                    format!("step {:?}: unknown variable {from:?}", step.id),
                ))
            }
        };
        if items.len() as u64 > u64::from(max) {
            return Err(RecipeError::new(
                ResolveErrorKind::RecipeBudgetExceeded,
                step_id,
                format!(
                    "step {:?}: bounded_each over {} item(s) exceeds max {max}",
                    step.id,
                    items.len()
                ),
            ));
        }
        // `item` is scoped to the nested steps; the previous binding (if any)
        // is restored afterwards so outer steps see a stable environment.
        let previous = self.vars.get(&as_name).cloned();
        for item in items {
            self.vars.insert(as_name.clone(), Var::Json(item));
            for nested_step in &nested {
                // `Box::pin` breaks the `execute_step -> dispatch ->
                // bounded_each -> execute_step` async recursion cycle.
                Box::pin(self.execute_step(nested_step, depth + 1)).await?;
            }
        }
        match previous {
            Some(value) => {
                self.vars.insert(as_name, value);
            }
            None => {
                self.vars.remove(&as_name);
            }
        }
        Ok(())
    }

    async fn op_http_request(
        &mut self,
        step: &RecipeStep,
        step_id: Option<&str>,
    ) -> Result<(), RecipeError> {
        let url_template = arg_str(&step.args, "url")?;
        let url = self.render(&url_template, step_id)?;
        let method = arg_str_optional(&step.args, "method").unwrap_or_else(|| "GET".to_owned());
        let max_bytes = step
            .args
            .get("max_bytes")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(64 * 1024);
        let mut spec = HttpRequestSpec {
            method,
            url,
            headers: BTreeMap::new(),
            body: None,
            max_bytes,
            follow_redirects: true,
        };
        if let Some(headers) = step.args.get("headers").and_then(Value::as_object) {
            for (name, value) in headers {
                let value = value.as_str().ok_or_else(|| {
                    RecipeError::new(
                        ResolveErrorKind::RecipeInvalid,
                        step_id,
                        format!("header {name} must be a string"),
                    )
                })?;
                spec.headers
                    .insert(name.clone(), self.render(value, step_id)?);
            }
        }
        let observation = self
            .http
            .request(spec)
            .await
            .map_err(|error| RecipeError::new(error.kind, step_id, error.detail))?;

        if let Some(expect) = &step.expect {
            if !expect.status_in.is_empty() && !expect.status_in.contains(&observation.status) {
                return Err(RecipeError::new(
                    ResolveErrorKind::RecipeStale,
                    step_id,
                    format!(
                        "step {:?} expected status {:?}, got {}",
                        step.id, expect.status_in, observation.status
                    ),
                ));
            }
            if !expect.content_type_prefix.is_empty() {
                let content_type = observation
                    .content_type()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let ok = expect
                    .content_type_prefix
                    .iter()
                    .any(|prefix| content_type.starts_with(&prefix.to_ascii_lowercase()));
                if !ok {
                    return Err(RecipeError::new(
                        ResolveErrorKind::RecipeStale,
                        step_id,
                        format!(
                            "step {:?} expected content-type {:?}, got {:?}",
                            step.id, expect.content_type_prefix, content_type
                        ),
                    ));
                }
            }
        }

        self.assign_from_observation(step, step_id, &observation)?;
        Ok(())
    }

    fn assign_from_observation(
        &mut self,
        step: &RecipeStep,
        step_id: Option<&str>,
        observation: &Observation,
    ) -> Result<(), RecipeError> {
        for (var, source) in &step.assign {
            let value = match source.as_str() {
                "body_json" => Var::Json(observation.body_json().ok_or_else(|| {
                    RecipeError::new(
                        ResolveErrorKind::RecipeStale,
                        step_id,
                        format!("step {:?}: response is not JSON", step.id),
                    )
                })?),
                "body_text" => Var::Text(observation.body_text()),
                "status" => Var::Text(observation.status.to_string()),
                "final_url" => Var::Text(observation.final_url.clone()),
                other if other.starts_with("header:") => {
                    let name = &other["header:".len()..];
                    Var::Text(observation.header(name).unwrap_or("").to_owned())
                }
                other => {
                    return Err(RecipeError::new(
                        ResolveErrorKind::RecipeInvalid,
                        step_id,
                        format!("unknown assign source {other:?}"),
                    ))
                }
            };
            self.vars.insert(var.clone(), value);
        }
        Ok(())
    }

    fn op_json_query(
        &mut self,
        step: &RecipeStep,
        step_id: Option<&str>,
    ) -> Result<(), RecipeError> {
        let from = arg_str(&step.args, "from")?;
        let pointer = arg_str(&step.args, "pointer")?;
        let value = self.vars.get(&from).cloned().ok_or_else(|| {
            RecipeError::new(
                ResolveErrorKind::RecipeInvalid,
                step_id,
                format!("step {:?}: unknown variable {from:?}", step.id),
            )
        })?;
        let Var::Json(json) = value else {
            return Err(RecipeError::new(
                ResolveErrorKind::RecipeInvalid,
                step_id,
                format!("step {:?}: {from:?} is not JSON", step.id),
            ));
        };
        let selected = json.pointer(&pointer).ok_or_else(|| {
            RecipeError::new(
                ResolveErrorKind::SelectorMiss,
                step_id,
                format!("step {:?}: JSON pointer {pointer:?} not found", step.id),
            )
        })?;
        self.vars
            .insert(arg_assign_name(step)?, Var::Json(selected.clone()));
        Ok(())
    }

    fn op_regex_capture(
        &mut self,
        step: &RecipeStep,
        step_id: Option<&str>,
    ) -> Result<(), RecipeError> {
        let from = arg_str(&step.args, "from")?;
        let pattern = arg_str(&step.args, "pattern")?;
        let group = step.args.get("group").and_then(Value::as_u64).unwrap_or(1) as usize;
        let text = self
            .vars
            .get(&from)
            .ok_or_else(|| {
                RecipeError::new(
                    ResolveErrorKind::RecipeInvalid,
                    step_id,
                    format!("step {:?}: unknown variable {from:?}", step.id),
                )
            })?
            .as_text();
        let regex = Regex::new(&pattern).map_err(|error| {
            RecipeError::new(
                ResolveErrorKind::RecipeInvalid,
                step_id,
                format!("step {:?}: invalid regex: {error}", step.id),
            )
        })?;
        let captures = regex.captures(&text).ok_or_else(|| {
            RecipeError::new(
                ResolveErrorKind::SelectorMiss,
                step_id,
                format!("step {:?}: pattern did not match", step.id),
            )
        })?;
        let value = captures
            .get(group)
            .map(|matched| matched.as_str())
            .ok_or_else(|| {
                RecipeError::new(
                    ResolveErrorKind::SelectorMiss,
                    step_id,
                    format!("step {:?}: capture group {group} missing", step.id),
                )
            })?;
        self.vars
            .insert(arg_assign_name(step)?, Var::Text(value.to_owned()));
        Ok(())
    }

    fn op_url_join(&mut self, step: &RecipeStep, step_id: Option<&str>) -> Result<(), RecipeError> {
        let base = self.render(&arg_str(&step.args, "base")?, step_id)?;
        let relative = self.render(&arg_str(&step.args, "relative")?, step_id)?;
        let base = Url::parse(&base).map_err(|error| {
            RecipeError::new(
                ResolveErrorKind::InvalidUrl,
                step_id,
                format!("step {:?}: invalid base URL: {error}", step.id),
            )
        })?;
        let joined = base.join(&relative).map_err(|error| {
            RecipeError::new(
                ResolveErrorKind::InvalidUrl,
                step_id,
                format!("step {:?}: cannot join URL: {error}", step.id),
            )
        })?;
        self.vars
            .insert(arg_assign_name(step)?, Var::Text(joined.to_string()));
        Ok(())
    }

    fn op_assert(&mut self, step: &RecipeStep, step_id: Option<&str>) -> Result<(), RecipeError> {
        let kind = arg_str(&step.args, "kind")?;
        let value = self.render(&arg_str(&step.args, "value")?, step_id)?;
        let failed = match kind.as_str() {
            "host_suffix" => {
                let suffix = arg_str(&step.args, "suffix")?;
                let host = Url::parse(&value)
                    .ok()
                    .and_then(|url| url.host_str().map(str::to_owned))
                    .unwrap_or_default();
                !host.ends_with(&suffix)
            }
            "contains" => {
                let needle = self.render(&arg_str(&step.args, "needle")?, step_id)?;
                !value.contains(&needle)
            }
            "equals" => {
                let expected = self.render(&arg_str(&step.args, "expected")?, step_id)?;
                value != expected
            }
            "matches" => {
                let pattern = assert_pattern(&step.args, &step.id).map_err(|error| {
                    RecipeError::new(ResolveErrorKind::RecipeInvalid, step_id, error)
                })?;
                let regex = Regex::new(pattern).map_err(|error| {
                    RecipeError::new(
                        ResolveErrorKind::RecipeInvalid,
                        step_id,
                        format!("step {:?}: invalid assert pattern: {error}", step.id),
                    )
                })?;
                !regex.is_match(&value)
            }
            other => {
                return Err(RecipeError::new(
                    ResolveErrorKind::RecipeInvalid,
                    step_id,
                    format!("unsupported assert kind {other:?}"),
                ))
            }
        };
        if failed {
            return Err(RecipeError::new(
                ResolveErrorKind::ExpectFailed,
                step_id,
                format!("step {:?}: assertion failed", step.id),
            ));
        }
        Ok(())
    }

    /// Substitute `{{var}}` placeholders (no expressions, no functions).
    fn render(&self, template: &str, step_id: Option<&str>) -> Result<String, RecipeError> {
        let mut output = String::with_capacity(template.len());
        let mut rest = template;
        while let Some(start) = rest.find("{{") {
            output.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else {
                return Err(RecipeError::new(
                    ResolveErrorKind::RecipeInvalid,
                    step_id,
                    "unterminated {{ placeholder",
                ));
            };
            let name = after[..end].trim();
            let value = self.vars.get(name).ok_or_else(|| {
                RecipeError::new(
                    ResolveErrorKind::RecipeInvalid,
                    step_id,
                    format!("unknown template variable {name:?}"),
                )
            })?;
            output.push_str(&value.as_text());
            rest = &after[end + 2..];
        }
        output.push_str(rest);
        Ok(output)
    }
}

/// Validate a condition (`when` / `branch`).
fn validate_condition(kind: &str, var: &str, expected: Option<&str>) -> Result<(), String> {
    if var.trim().is_empty() {
        return Err("condition `var` must not be empty".to_owned());
    }
    match kind {
        "exists" => Ok(()),
        "equals" | "contains" => {
            if expected.is_none_or(|value| value.is_empty()) {
                Err(format!("condition kind {kind:?} requires `expected`"))
            } else {
                Ok(())
            }
        }
        other => Err(format!(
            "unsupported condition kind {other:?} (expected exists|equals|contains)"
        )),
    }
}

/// `true` when the step assigns exactly one variable (ops that write a value).
fn require_single_assign(step: &RecipeStep) -> Result<(), String> {
    if step.assign.len() == 1 {
        Ok(())
    } else {
        Err(format!(
            "step {:?} must assign exactly one variable",
            step.id
        ))
    }
}

/// Regex pattern of an `assert` step with `kind = "matches"`.
fn assert_pattern<'a>(args: &'a Value, step_id: &str) -> Result<&'a str, String> {
    args.get("pattern")
        .or_else(|| args.get("regex"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!("step {step_id:?}: assert `matches` requires a string `pattern` argument")
        })
}

/// Deserialize the nested `steps` of a `bounded_each` step.
fn parse_nested_steps(args: &Value, step_id: &str) -> Result<Vec<RecipeStep>, String> {
    let value = args.get("steps").ok_or_else(|| {
        format!("step {step_id:?}: bounded_each requires an array `steps` argument")
    })?;
    let nested: Vec<RecipeStep> = serde_json::from_value(value.clone())
        .map_err(|error| format!("step {step_id:?}: invalid nested steps: {error}"))?;
    if nested.is_empty() {
        return Err(format!(
            "step {step_id:?}: bounded_each requires at least one nested step"
        ));
    }
    Ok(nested)
}

fn arg_str(args: &Value, name: &str) -> Result<String, RecipeError> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            RecipeError::new(
                ResolveErrorKind::RecipeInvalid,
                None,
                format!("missing string argument {name:?}"),
            )
        })
}

fn arg_str_optional(args: &Value, name: &str) -> Option<String> {
    args.get(name).and_then(Value::as_str).map(str::to_owned)
}

fn arg_assign_name(step: &RecipeStep) -> Result<String, RecipeError> {
    match step.assign.len() {
        1 => Ok(step
            .assign
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(String::new)),
        _ => Err(RecipeError::new(
            ResolveErrorKind::RecipeInvalid,
            Some(step.id.as_str()),
            format!(
                "step {:?} must assign exactly one variable, got {}",
                step.id,
                step.assign.len()
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECIPE_JSON: &str = r#"{
        "schema": "rdm.recipe/v1",
        "id": "example.com-file",
        "version": 1,
        "name": "Example",
        "match": { "hosts": ["example.com"], "url_pattern": "^https://example\\.com/d/(?P<id>\\d+)$" },
        "limits": { "requests": 4, "bytes_read": 65536, "wall_ms": 5000, "redirects": 2 },
        "steps": [
            {
                "id": "meta",
                "op": "http_request",
                "args": { "method": "GET", "url": "https://api.example.com/d/{{id}}" },
                "expect": { "status_in": [200], "content_type_prefix": ["application/json"] },
                "assign": { "meta": "body_json" }
            },
            {
                "id": "url",
                "op": "json_query",
                "args": { "from": "meta", "pointer": "/file/url" },
                "assign": { "download_url": "value" }
            }
        ],
        "emit": { "url": "{{download_url}}", "name": "{{id}}.bin", "one_time": true }
    }"#;

    #[test]
    fn parses_and_validates() {
        let recipe = Recipe::from_json(RECIPE_JSON).unwrap();
        assert_eq!(recipe.id, "example.com-file");
        assert_eq!(recipe.steps.len(), 2);
        assert_eq!(recipe.specificity(), 20);
    }

    #[test]
    fn rejects_unknown_operator() {
        let broken = RECIPE_JSON.replace("\"json_query\"", "\"eval_js\"");
        assert!(Recipe::from_json(&broken).is_err());
    }

    #[test]
    fn match_captures_named_groups() {
        let recipe = Recipe::from_json(RECIPE_JSON).unwrap();
        let url = Url::parse("https://example.com/d/12345").unwrap();
        let vars = recipe.matches(&url).unwrap();
        assert_eq!(vars.get("id").map(String::as_str), Some("12345"));
        assert!(recipe
            .matches(&Url::parse("https://example.com/about").unwrap())
            .is_none());
        assert!(recipe
            .matches(&Url::parse("https://evil.com/d/1").unwrap())
            .is_none());
    }

    #[test]
    fn budget_is_clamped_by_outer_budget() {
        let recipe = Recipe::from_json(RECIPE_JSON).unwrap();
        let outer = Budget {
            wall_ms: 1000,
            http_requests: 2,
            bytes_read: 1024,
            redirects: 1,
        };
        let budget = recipe.budget_within(outer);
        assert_eq!(budget.wall_ms, 1000);
        assert_eq!(budget.http_requests, 2);
        assert_eq!(budget.bytes_read, 1024);
        assert_eq!(budget.redirects, 1);
    }

    #[test]
    fn new_step_fields_are_backward_compatible() {
        let recipe: Recipe = serde_json::from_str(RECIPE_JSON).unwrap();
        assert!(recipe.steps.iter().all(|step| step.when.is_none()));
        assert!(recipe.steps.iter().all(|step| step.on_fail.is_none()));
        assert!(recipe.tests.is_empty());
        recipe.validate().unwrap();
    }

    // ------------------------------------------------------------------
    // Operators: template / branch / when / assert matches
    // ------------------------------------------------------------------

    fn http_tool() -> HttpTool {
        HttpTool::new(crate::policy::ResolvePolicy::loopback(), Budget::default())
            .expect("http tool")
    }

    #[tokio::test]
    async fn template_and_branch_set_variables_for_later_steps() {
        let recipe = Recipe::from_json(
            r#"{
                "schema": "rdm.recipe/v1",
                "id": "template-branch",
                "version": 1,
                "match": {},
                "steps": [
                    {"id": "t", "op": "template",
                     "args": {"template": "{{a}}-{{b}}"}, "assign": {"combined": "value"}},
                    {"id": "has", "op": "branch",
                     "args": {"kind": "exists", "var": "a"}, "assign": {"present": "value"}},
                    {"id": "eq", "op": "branch",
                     "args": {"kind": "equals", "var": "a", "expected": "nope"},
                     "assign": {"same": "value"}},
                    {"id": "check1", "op": "assert",
                     "args": {"kind": "equals", "value": "{{combined}}", "expected": "1-2"}},
                    {"id": "check2", "op": "assert",
                     "args": {"kind": "equals", "value": "{{present}}", "expected": "true"}},
                    {"id": "check3", "op": "assert",
                     "args": {"kind": "equals", "value": "{{same}}", "expected": "false"}}
                ],
                "emit": {"url": "https://example.com/{{combined}}.bin"}
            }"#,
        )
        .unwrap();

        let mut http = http_tool();
        let mut runner = RecipeRunner::new(&mut http, &recipe);
        runner.seed(BTreeMap::from([
            ("a".to_owned(), "1".to_owned()),
            ("b".to_owned(), "2".to_owned()),
        ]));
        let output = runner.run().await.unwrap();
        assert_eq!(output.url, "https://example.com/1-2.bin");
        assert_eq!(runner.steps_run(), 6);
    }

    #[tokio::test]
    async fn when_guard_skips_steps_without_failing() {
        let recipe = Recipe::from_json(
            r#"{
                "schema": "rdm.recipe/v1",
                "id": "when-guard",
                "version": 1,
                "match": {},
                "steps": [
                    {"id": "guarded_exists", "op": "assert",
                     "when": {"kind": "exists", "var": "missing"},
                     "args": {"kind": "equals", "value": "a", "expected": "b"}},
                    {"id": "guarded_equals", "op": "assert",
                     "when": {"kind": "equals", "var": "missing", "expected": "x"},
                     "args": {"kind": "equals", "value": "a", "expected": "b"}},
                    {"id": "runs", "op": "assert",
                     "when": {"kind": "exists", "var": "present"},
                     "args": {"kind": "equals", "value": "x", "expected": "x"}}
                ],
                "emit": {"url": "https://example.com/f.bin"}
            }"#,
        )
        .unwrap();

        let mut http = http_tool();
        let mut runner = RecipeRunner::new(&mut http, &recipe);
        runner.seed(BTreeMap::from([("present".to_owned(), "1".to_owned())]));
        runner.run().await.unwrap();
        assert_eq!(runner.steps_run(), 1);
    }

    #[test]
    fn validate_rejects_invalid_when_guard() {
        let text = r#"{
            "schema": "rdm.recipe/v1", "id": "bad-when", "version": 1, "match": {},
            "steps": [{"id": "s", "op": "assert",
                       "when": {"kind": "regex", "var": "x"},
                       "args": {"kind": "equals", "value": "x", "expected": "x"}}],
            "emit": {"url": "https://example.com/f.bin"}
        }"#;
        assert!(Recipe::from_json(text)
            .unwrap_err()
            .contains("condition kind"));
    }

    #[tokio::test]
    async fn assert_matches_supports_regular_expressions() {
        let recipe = Recipe::from_json(
            r#"{
                "schema": "rdm.recipe/v1",
                "id": "assert-matches",
                "version": 1,
                "match": {},
                "steps": [{"id": "check", "op": "assert",
                           "args": {"kind": "matches", "value": "{{url}}",
                                    "pattern": "^https://cdn\\.example\\.com/[0-9]+\\.bin$"}}],
                "emit": {"url": "https://example.com/f.bin"}
            }"#,
        )
        .unwrap();

        let mut http = http_tool();
        let mut runner = RecipeRunner::new(&mut http, &recipe);
        runner.seed(BTreeMap::from([(
            "url".to_owned(),
            "https://cdn.example.com/42.bin".to_owned(),
        )]));
        runner.run().await.unwrap();

        let mut http = http_tool();
        let mut runner = RecipeRunner::new(&mut http, &recipe);
        runner.seed(BTreeMap::from([(
            "url".to_owned(),
            "ftp://cdn.example.com/42.bin".to_owned(),
        )]));
        let error = runner.run().await.unwrap_err();
        assert_eq!(error.kind, ResolveErrorKind::ExpectFailed);
        assert_eq!(error.step_id.as_deref(), Some("check"));
    }

    #[test]
    fn validate_rejects_invalid_assert_regex() {
        let text = r#"{
            "schema": "rdm.recipe/v1", "id": "bad-regex", "version": 1, "match": {},
            "steps": [{"id": "s", "op": "assert",
                       "args": {"kind": "matches", "value": "x", "pattern": "("}}],
            "emit": {"url": "https://example.com/f.bin"}
        }"#;
        assert!(Recipe::from_json(text).unwrap_err().contains("pattern"));
    }

    // ------------------------------------------------------------------
    // on_fail
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn on_fail_overrides_the_error_kind() {
        let recipe = Recipe::from_json(
            r#"{
                "schema": "rdm.recipe/v1", "id": "on-fail", "version": 1, "match": {},
                "steps": [{"id": "bad", "op": "assert", "on_fail": "recipe_invalid",
                           "args": {"kind": "equals", "value": "a", "expected": "b"}}],
                "emit": {"url": "https://example.com/f.bin"}
            }"#,
        )
        .unwrap();

        let mut http = http_tool();
        let mut runner = RecipeRunner::new(&mut http, &recipe);
        let error = runner.run().await.unwrap_err();
        assert_eq!(error.kind, ResolveErrorKind::RecipeInvalid);
        assert_eq!(error.step_id.as_deref(), Some("bad"));
    }

    #[test]
    fn validate_rejects_unknown_on_fail() {
        let text = r#"{
            "schema": "rdm.recipe/v1", "id": "bad-on-fail", "version": 1, "match": {},
            "steps": [{"id": "s", "op": "assert", "on_fail": "not_a_kind",
                       "args": {"kind": "equals", "value": "x", "expected": "x"}}],
            "emit": {"url": "https://example.com/f.bin"}
        }"#;
        assert!(Recipe::from_json(text).unwrap_err().contains("on_fail"));
    }

    #[test]
    fn on_fail_accepts_pascal_case_alias() {
        let text = r#"{
            "schema": "rdm.recipe/v1", "id": "pascal-on-fail", "version": 1, "match": {},
            "steps": [{"id": "s", "op": "assert", "on_fail": "RecipeInvalid",
                       "args": {"kind": "equals", "value": "x", "expected": "x"}}],
            "emit": {"url": "https://example.com/f.bin"}
        }"#;
        Recipe::from_json(text).unwrap();
    }

    // ------------------------------------------------------------------
    // bounded_each
    // ------------------------------------------------------------------

    const BOUNDED_EACH_JSON: &str = r#"{
        "schema": "rdm.recipe/v1",
        "id": "bounded-each",
        "version": 1,
        "match": {},
        "steps": [
            {"id": "loop", "op": "bounded_each",
             "args": {"from": "items", "max": 5, "as": "item", "steps": [
                {"id": "inner_template", "op": "template",
                 "args": {"template": "https://cdn.example.com/{{item}}.bin"},
                 "assign": {"joined": "value"}},
                {"id": "inner_assert", "op": "assert",
                 "args": {"kind": "matches", "value": "{{joined}}",
                          "pattern": "^https://cdn\\.example\\.com/[ab]\\.bin$"}}
             ]}},
            {"id": "after", "op": "assert",
             "args": {"kind": "equals", "value": "{{joined}}",
                      "expected": "https://cdn.example.com/b.bin"}}
        ],
        "emit": {"url": "{{joined}}"}
    }"#;

    #[tokio::test]
    async fn bounded_each_runs_nested_steps_for_each_item() {
        let recipe = Recipe::from_json(BOUNDED_EACH_JSON).unwrap();
        let mut http = http_tool();
        let mut runner = RecipeRunner::new(&mut http, &recipe);
        runner
            .vars
            .insert("items".to_owned(), Var::Json(serde_json::json!(["a", "b"])));
        let output = runner.run().await.unwrap();
        assert_eq!(output.url, "https://cdn.example.com/b.bin");
        // loop + 2 nested steps per item + trailing assert
        assert_eq!(runner.steps_run(), 6);
    }

    #[tokio::test]
    async fn bounded_each_enforces_hard_maximum() {
        let recipe = Recipe::from_json(BOUNDED_EACH_JSON).unwrap();
        let mut http = http_tool();
        let mut runner = RecipeRunner::new(&mut http, &recipe);
        runner.vars.insert(
            "items".to_owned(),
            Var::Json(serde_json::json!(["a", "b", "a", "b", "a", "b"])),
        );
        let error = runner.run().await.unwrap_err();
        assert_eq!(error.kind, ResolveErrorKind::RecipeBudgetExceeded);
        assert_eq!(error.step_id.as_deref(), Some("loop"));
    }

    #[tokio::test]
    async fn bounded_each_requires_a_json_array() {
        let recipe = Recipe::from_json(BOUNDED_EACH_JSON).unwrap();
        let mut http = http_tool();
        let mut runner = RecipeRunner::new(&mut http, &recipe);
        runner.seed(BTreeMap::from([("items".to_owned(), "nope".to_owned())]));
        let error = runner.run().await.unwrap_err();
        assert_eq!(error.kind, ResolveErrorKind::RecipeInvalid);
        assert!(error.detail.contains("JSON array"));
    }

    #[test]
    fn validate_enforces_bounded_each_max_range() {
        for max in [0_u32, 101_u32] {
            let text = format!(
                r#"{{"schema": "rdm.recipe/v1", "id": "m", "version": 1, "match": {{}},
                    "steps": [{{"id": "loop", "op": "bounded_each",
                               "args": {{"from": "items", "max": {max}, "steps": [
                                   {{"id": "leaf", "op": "assert",
                                     "args": {{"kind": "equals", "value": "x", "expected": "x"}}}}]}}}}],
                    "emit": {{"url": "https://example.com/f.bin"}}}}"#
            );
            let error = Recipe::from_json(&text).unwrap_err();
            assert!(error.contains("max"), "{error}");
        }
    }

    #[test]
    fn validate_rejects_empty_bounded_each_steps() {
        let text = r#"{
            "schema": "rdm.recipe/v1", "id": "empty-each", "version": 1, "match": {},
            "steps": [{"id": "loop", "op": "bounded_each",
                       "args": {"from": "items", "steps": []}}],
            "emit": {"url": "https://example.com/f.bin"}
        }"#;
        assert!(Recipe::from_json(text)
            .unwrap_err()
            .contains("at least one nested step"));
    }

    #[test]
    fn validate_rejects_unknown_operator_in_nested_steps() {
        let text = r#"{
            "schema": "rdm.recipe/v1", "id": "nested-op", "version": 1, "match": {},
            "steps": [{"id": "loop", "op": "bounded_each",
                       "args": {"from": "items", "steps": [
                           {"id": "leaf", "op": "eval_js"}]}}],
            "emit": {"url": "https://example.com/f.bin"}
        }"#;
        assert!(Recipe::from_json(text)
            .unwrap_err()
            .contains("unsupported operator"));
    }

    #[test]
    fn validate_rejects_duplicate_nested_step_ids() {
        let text = r#"{
            "schema": "rdm.recipe/v1", "id": "duplicate", "version": 1, "match": {},
            "steps": [{"id": "loop", "op": "bounded_each",
                       "args": {"from": "items", "steps": [
                           {"id": "loop", "op": "assert",
                            "args": {"kind": "equals", "value": "x", "expected": "x"}}]}}],
            "emit": {"url": "https://example.com/f.bin"}
        }"#;
        assert!(Recipe::from_json(text)
            .unwrap_err()
            .contains("duplicate step id"));
    }

    fn nested_each_recipe(levels: usize) -> String {
        let mut steps = serde_json::json!([
            {"id": "leaf", "op": "assert",
             "args": {"kind": "equals", "value": "x", "expected": "x"}}
        ]);
        for level in 0..levels {
            steps = serde_json::json!([{
                "id": format!("each_{level}"),
                "op": "bounded_each",
                "args": {"from": "items", "steps": steps}
            }]);
        }
        serde_json::json!({
            "schema": RECIPE_SCHEMA,
            "id": "deep-each",
            "version": 1,
            "match": {},
            "steps": steps,
            "emit": {"url": "https://example.com/f.bin"}
        })
        .to_string()
    }

    #[test]
    fn validate_enforces_bounded_each_depth() {
        assert!(Recipe::from_json(&nested_each_recipe(MAX_BOUNDED_EACH_DEPTH as usize)).is_ok());
        let error = Recipe::from_json(&nested_each_recipe(MAX_BOUNDED_EACH_DEPTH as usize + 1))
            .unwrap_err();
        assert!(error.contains("nesting"), "{error}");
    }

    // ------------------------------------------------------------------
    // Recipe self-tests
    // ------------------------------------------------------------------

    #[test]
    fn check_tests_reports_match_no_match_skip_and_invalid_skip() {
        let recipe = Recipe::from_json(
            r#"{
                "schema": "rdm.recipe/v1",
                "id": "self-test",
                "version": 1,
                "match": {"hosts": ["example.com"],
                          "url_pattern": "^https://example\\.com/d/(?P<id>\\d+)$"},
                "steps": [{"id": "s", "op": "assert",
                           "args": {"kind": "equals", "value": "x", "expected": "x"}}],
                "emit": {"url": "https://example.com/f.bin"},
                "tests": [
                    {"name": "matches", "input_url": "https://example.com/d/1"},
                    {"name": "must-not-match", "input_url": "https://example.com/about",
                     "expect_no_match": true},
                    {"name": "must-match", "input_url": "https://example.com/about"},
                    {"name": "needs-login", "input_url": "https://example.com/d/2",
                     "skip": "login required"},
                    {"name": "bad-skip", "input_url": "https://example.com/d/3",
                     "skip": "   "}
                ]
            }"#,
        )
        .unwrap();

        let results = recipe.check_tests();
        assert_eq!(results.len(), 5);
        assert_eq!(results[0].name, "matches");
        assert!(results[0].ok && !results[0].skipped);
        assert!(results[1].ok && !results[1].skipped);
        assert!(!results[2].ok && !results[2].skipped);
        assert!(results[3].ok && results[3].skipped);
        assert!(results[3].message.contains("login required"));
        assert!(!results[4].ok && !results[4].skipped);
        assert!(results[4].message.contains("skip reason"));
    }
}

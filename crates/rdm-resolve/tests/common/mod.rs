//! Shared helpers for the `rdm-resolve` integration test matrix.
//!
//! Scratch directories live under Cargo's `CARGO_TARGET_TMPDIR` (inside the
//! workspace `target/`) instead of the OS temp dir. The sandboxed test runner
//! may deny writes to `%LOCALAPPDATA%\Temp`, and keeping fixtures inside the
//! target tree makes the suite hermetic and reproducible.

#![allow(dead_code)]

pub mod llm;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

use rdm_resolve::{
    Budget, HttpTool, Recipe, RecipeError, RecipeOutput, RecipeRegistry, RecipeRunner,
    ResolvePolicy, ResolveRequest, ResolverService, Verification, Verifier, VerifyHints,
};

/// Network policy that permits the loopback mock servers.
pub fn loopback_policy() -> ResolvePolicy {
    ResolvePolicy::loopback()
}

/// Budget generous enough for a handful of loopback requests.
pub fn test_budget() -> Budget {
    Budget {
        wall_ms: 30_000,
        ..Budget::default()
    }
}

/// Build an [`HttpTool`] for the loopback fixtures.
pub fn http_tool(policy: ResolvePolicy, budget: Budget) -> HttpTool {
    HttpTool::new(policy, budget).expect("build HttpTool")
}

/// Verify `url` with an explicit policy and budget.
pub async fn verify_with(
    url: &str,
    hints: &VerifyHints,
    policy: ResolvePolicy,
    budget: Budget,
) -> Verification {
    let mut http = http_tool(policy, budget);
    Verifier::new(&mut http).verify(url, hints).await
}

/// Verify `url` against a loopback fixture with the default hints.
pub async fn verify(url: &str) -> Verification {
    verify_with(
        url,
        &VerifyHints::default(),
        loopback_policy(),
        test_budget(),
    )
    .await
}

/// A plain resolution request for `url` (mode defaults to `auto`).
pub fn resolve_request(url: &str) -> ResolveRequest {
    ResolveRequest {
        url: url.to_owned(),
        ..ResolveRequest::default()
    }
}

/// Seed variables for a [`RecipeRunner`].
pub fn seed(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

/// Run a recipe directly (no registry/service) with seeded variables.
///
/// The returned HTTP tool is idle for recipes whose steps never issue a
/// request, which keeps operator tests independent from a fixture server.
pub async fn run_recipe(
    recipe: &Recipe,
    variables: BTreeMap<String, String>,
) -> Result<RecipeOutput, RecipeError> {
    let mut http = http_tool(loopback_policy(), test_budget());
    let mut runner = RecipeRunner::new(&mut http, recipe);
    runner.seed(variables);
    runner.run().await
}

/// Like [`run_recipe`], but also reports how many steps were executed.
///
/// Skipped steps (a false `when` guard) are not counted.
pub async fn run_recipe_counted(
    recipe: &Recipe,
    variables: BTreeMap<String, String>,
) -> Result<(RecipeOutput, u32), RecipeError> {
    let mut http = http_tool(loopback_policy(), test_budget());
    let mut runner = RecipeRunner::new(&mut http, recipe);
    runner.seed(variables);
    let output = runner.run().await?;
    Ok((output, runner.steps_run()))
}

/// Build a valid `rdm.recipe/v1` JSON document.
pub fn recipe_json(id: &str, version: u32, matcher: Value, steps: Value, emit: Value) -> String {
    serde_json::json!({
        "schema": "rdm.recipe/v1",
        "id": id,
        "version": version,
        "match": matcher,
        "steps": steps,
        "emit": emit,
    })
    .to_string()
}

/// Parse a recipe built with [`recipe_json`] (panics on invalid fixtures).
pub fn recipe_value(id: &str, version: u32, matcher: Value, steps: Value, emit: Value) -> Recipe {
    Recipe::from_json(&recipe_json(id, version, matcher, steps, emit))
        .expect("fixture recipe must be valid")
}

/// Scratch directory under `target/tmp`, removed on drop.
pub struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    pub fn new(tag: &str) -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let base = option_env!("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!(
            "rdm-resolve-it-{tag}-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, relative: &str) -> PathBuf {
        self.path.join(relative)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A resolver whose on-disk recipe library is pre-seeded with fixtures.
///
/// The layout matches [`ResolverService::with_state_dir`]:
/// `<scratch>/resolve/recipes/<id>@<version>.json`.
pub struct RecipeFixture {
    scratch: ScratchDir,
}

impl RecipeFixture {
    pub fn new(tag: &str) -> Self {
        Self {
            scratch: ScratchDir::new(tag),
        }
    }

    /// Validate, persist and return one recipe fixture.
    pub fn save(&self, recipe_json: &str) -> Recipe {
        let recipe = Recipe::from_json(recipe_json).expect("fixture recipe must be valid");
        self.save_recipe(&recipe);
        recipe
    }

    /// Persist an already-built recipe.
    pub fn save_recipe(&self, recipe: &Recipe) -> PathBuf {
        let recipes_dir = self.recipes_dir();
        let registry = RecipeRegistry::with_root(&recipes_dir);
        registry.save(recipe).expect("save fixture recipe")
    }

    pub fn recipes_dir(&self) -> PathBuf {
        self.scratch.join("resolve").join("recipes")
    }

    pub fn state_dir(&self) -> &Path {
        self.scratch.path()
    }

    /// Service that loads the on-disk fixtures with the loopback policy.
    pub fn service(&self) -> ResolverService {
        ResolverService::with_state_dir(self.state_dir(), loopback_policy())
    }

    /// Service with a caller-chosen policy (e.g. an empty recipe library).
    pub fn service_with_policy(&self, policy: ResolvePolicy) -> ResolverService {
        ResolverService::with_state_dir(self.state_dir(), policy)
    }
}

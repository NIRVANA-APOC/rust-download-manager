//! On-disk recipe registry + method index.
//!
//! Layout (`docs/architecture.md` §11.10):
//!
//! ```text
//! <state>/resolve/recipes/<safe-id>@<version>.json   # recipe files
//! <state>/resolve/index.json                          # active_version + health
//! ```
//!
//! Loading is tolerant (an invalid recipe/index file is skipped or replaced by
//! an empty index with a warning instead of discarding the whole library), and
//! saving is atomic so a crash cannot truncate an existing file.
//!
//! `RecipeRegistry::with_root` keeps taking the *recipes* directory; when that
//! directory is literally named `recipes` the index lives next to it
//! (`<state>/resolve/index.json`, the documented layout), otherwise the index
//! lives inside the given root (`<root>/index.json`, convenient for tests and
//! custom layouts).

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::contract::{now_ms, ResolveErrorKind};
use crate::health::RecipeHealth;
use crate::recipe::Recipe;

/// Schema identifier of `<state>/resolve/index.json`.
///
/// v2 adds the optional per-version `versions` map; v1 files stay loadable
/// (their `health` field is treated as the health of whatever active version
/// the entry names) and are rewritten as v2 on the next save.
pub const INDEX_SCHEMA: &str = "rdm.recipe-index/v2";

/// One recipe selected for a URL.
#[derive(Debug, Clone)]
pub struct MatchHit<'a> {
    pub recipe: &'a Recipe,
    pub vars: BTreeMap<String, String>,
    pub specificity: usize,
}

/// Index entry of one recipe id: matcher pin + per-version health.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeIndexEntry {
    /// Version the matcher is allowed to pick; `set_active_version` can move it
    /// back to an older version (rollback) without touching recipe files.
    #[serde(default)]
    pub active_version: u32,
    /// Mirror of `versions[active_version]` (v0.15 keeps the old single-health
    /// field for backward compatibility; always equals [`Self::health_for`]'s
    /// active entry).
    #[serde(default)]
    pub health: RecipeHealth,
    /// Health per recorded recipe version (`{"1": RecipeHealth, ...}` in JSON).
    /// Absent in v1 files; filled on the next record/rollback/insert.
    #[serde(default)]
    pub versions: BTreeMap<u32, RecipeHealth>,
    #[serde(default)]
    pub updated_ms: u64,
}

/// Persisted `<state>/resolve/index.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeIndex {
    #[serde(default = "default_index_schema")]
    pub schema: String,
    #[serde(default)]
    pub updated_ms: u64,
    #[serde(default)]
    pub entries: BTreeMap<String, RecipeIndexEntry>,
}

impl Default for RecipeIndex {
    fn default() -> Self {
        Self {
            schema: INDEX_SCHEMA.to_owned(),
            updated_ms: 0,
            entries: BTreeMap::new(),
        }
    }
}

fn default_index_schema() -> String {
    INDEX_SCHEMA.to_owned()
}

/// In-memory recipe library + method index with an optional disk root.
#[derive(Debug, Clone, Default)]
pub struct RecipeRegistry {
    recipes: Vec<Recipe>,
    root: Option<PathBuf>,
    index: RecipeIndex,
}

impl RecipeRegistry {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let mut registry = Self::load(&root);
        registry.root = Some(root);
        registry
    }

    /// Load every `*.json` recipe and `index.json` under `root`.
    pub fn load(root: &Path) -> Self {
        let mut registry = Self {
            recipes: Vec::new(),
            root: Some(root.to_path_buf()),
            index: RecipeIndex::default(),
        };
        registry.index = load_index(root);
        let Ok(entries) = std::fs::read_dir(root) else {
            return registry;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            // `index.json` can live inside a custom root; never treat it as a
            // recipe file (the documented layout keeps it one level up).
            if path.file_name().and_then(|name| name.to_str()) == Some("index.json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                tracing::warn!(path = %path.display(), "rdm-resolve: cannot read recipe");
                continue;
            };
            match Recipe::from_json(&text) {
                Ok(recipe) => registry.recipes.push(recipe),
                Err(error) => {
                    tracing::warn!(path = %path.display(), error = %error, "rdm-resolve: skipping invalid recipe")
                }
            }
        }
        registry
    }

    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Absolute path of the persisted index, when a root is configured.
    pub fn index_path(&self) -> Option<PathBuf> {
        self.root.as_deref().map(index_path)
    }

    /// Read-only view of the persisted index.
    pub fn index(&self) -> &RecipeIndex {
        &self.index
    }

    /// Health of the recipe's **active** version (`Untested` when nothing was
    /// recorded). Backward compatible single-health view; see
    /// [`Self::health_for`] for one specific version.
    pub fn health(&self, id: &str) -> RecipeHealth {
        let active = self
            .index
            .entries
            .get(id)
            .map(|entry| entry.active_version)
            .unwrap_or(0);
        self.health_for(id, active)
    }

    /// Health recorded for one exact recipe version.
    ///
    /// Lookup order (v0.15 migration semantics):
    /// 1. `versions[version]` when that version was ever recorded;
    /// 2. `entry.health` when `version == active_version` (v1 files: the old
    ///    single health field belongs to the active version);
    /// 3. [`RecipeHealth::default`] otherwise.
    pub fn health_for(&self, id: &str, version: u32) -> RecipeHealth {
        let Some(entry) = self.index.entries.get(id) else {
            return RecipeHealth::default();
        };
        if let Some(health) = entry.versions.get(&version) {
            return health.clone();
        }
        if entry.active_version == version {
            entry.health.clone()
        } else {
            RecipeHealth::default()
        }
    }

    /// Recorded per-version health map of one recipe id (`None` when the index
    /// has no entry; the map may still be empty after a v1 migration).
    pub fn health_by_version(&self, id: &str) -> Option<&BTreeMap<u32, RecipeHealth>> {
        self.index.entries.get(id).map(|entry| &entry.versions)
    }

    /// Version the matcher may pick for `id`, if the index has an entry.
    pub fn active_version(&self, id: &str) -> Option<u32> {
        self.index.entries.get(id).map(|entry| entry.active_version)
    }

    pub fn all(&self) -> &[Recipe] {
        &self.recipes
    }

    pub fn len(&self) -> usize {
        self.recipes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.recipes.is_empty()
    }

    /// Validate + insert; an existing `(id, version)` pair is replaced.
    ///
    /// The in-memory index promotes `active_version` to `max(existing, version)`
    /// (a brand-new entry starts at the newest version of that id). A new
    /// version starts `untested`: before promoting, the previous active
    /// version's non-default health is solidified into `versions` (existing
    /// history is never overwritten), then `health` mirrors the new active
    /// version. Persisting the index is explicit via [`Self::save_index`] —
    /// recipe files are written by [`Self::save`].
    pub fn insert(&mut self, recipe: Recipe) -> Result<(), String> {
        recipe.validate()?;
        let id = recipe.id.clone();
        let version = recipe.version;
        self.recipes
            .retain(|existing| !(existing.id == recipe.id && existing.version == recipe.version));
        self.recipes.push(recipe);
        self.recipes
            .sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.version.cmp(&b.version)));

        let newest = self.latest_version(&id).unwrap_or(version);
        let now = now_ms();
        let entry = self
            .index
            .entries
            .entry(id)
            .or_insert_with(|| RecipeIndexEntry {
                active_version: newest,
                updated_ms: now,
                ..RecipeIndexEntry::default()
            });
        // Preserve the old active version's history before promoting a newer one
        // (a new version must not inherit the old version's failures).
        let previous_active = entry.active_version;
        let previous_health = (entry.health != RecipeHealth::default())
            .then(|| (previous_active, entry.health.clone()));
        if let Some((version, health)) = previous_health {
            entry.versions.entry(version).or_insert(health);
        }
        entry.active_version = entry.active_version.max(version);
        entry.health = entry
            .versions
            .get(&entry.active_version)
            .cloned()
            .unwrap_or_default();
        entry.updated_ms = now;
        self.index.updated_ms = now;
        Ok(())
    }

    /// Look up one recipe; `version = None` returns the newest version
    /// (explicit replay/debugging keeps working even for non-active versions).
    pub fn get(&self, id: &str, version: Option<u32>) -> Option<&Recipe> {
        match version {
            Some(version) => self
                .recipes
                .iter()
                .find(|recipe| recipe.id == id && recipe.version == version),
            None => self
                .recipes
                .iter()
                .filter(|recipe| recipe.id == id)
                .max_by_key(|recipe| recipe.version),
        }
    }

    /// Best recipe for `url`: priority, then applicability specificity, then
    /// newest version. Only the `active_version` of each recipe id participates
    /// (no index entry → newest available version).
    pub fn find_for_url(&self, url: &Url) -> Option<MatchHit<'_>> {
        let mut best: Option<MatchHit<'_>> = None;
        for recipe in &self.recipes {
            if !self.is_active(recipe) {
                continue;
            }
            let Some(vars) = recipe.matches(url) else {
                continue;
            };
            let specificity = recipe.specificity();
            let candidate = MatchHit {
                recipe,
                vars,
                specificity,
            };
            let replace = match &best {
                None => true,
                Some(current) => {
                    let candidate_key = (
                        candidate.recipe.matcher.priority,
                        candidate.specificity,
                        candidate.recipe.version,
                    );
                    let current_key = (
                        current.recipe.matcher.priority,
                        current.specificity,
                        current.recipe.version,
                    );
                    candidate_key > current_key
                }
            };
            if replace {
                best = Some(candidate);
            }
        }
        best
    }

    /// Persist one recipe under the registry root.
    pub fn save(&self, recipe: &Recipe) -> io::Result<PathBuf> {
        recipe
            .validate()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let root = self
            .root
            .as_ref()
            .ok_or_else(|| io::Error::other("recipe registry has no root directory"))?;
        std::fs::create_dir_all(root)?;
        let path = root.join(recipe_file_name(recipe));
        let text = serde_json::to_string_pretty(recipe)
            .map_err(|error| io::Error::other(format!("cannot serialize recipe: {error}")))?;
        write_atomic(&path, &text)?;
        Ok(path)
    }

    /// Move the matcher to another existing version (rollback / pin).
    ///
    /// Only the index is rewritten; recipe files stay untouched. After the
    /// switch `entry.health` mirrors `health_for(id, version)` (a version
    /// without recorded health starts `untested`), while all per-version
    /// history is retained. The previous in-memory index is restored when
    /// saving fails.
    pub fn set_active_version(&mut self, id: &str, version: u32) -> Result<(), String> {
        if self.get(id, Some(version)).is_none() {
            return Err(format!(
                "recipe {id:?} version {version} does not exist in the registry"
            ));
        }
        let previous = self.index.clone();
        let now = now_ms();
        let entry = self
            .index
            .entries
            .entry(id.to_owned())
            .or_insert_with(|| RecipeIndexEntry {
                active_version: version,
                updated_ms: now,
                ..RecipeIndexEntry::default()
            });
        if entry.active_version != version {
            // Solidify the current mirror first (v1 migration: old files only
            // have `health`, no `versions` map) without overwriting history.
            let current = (entry.health != RecipeHealth::default())
                .then(|| (entry.active_version, entry.health.clone()));
            if let Some((current_version, health)) = current {
                entry.versions.entry(current_version).or_insert(health);
            }
            entry.active_version = version;
            entry.health = entry.versions.get(&version).cloned().unwrap_or_default();
        }
        entry.updated_ms = now;
        self.index.updated_ms = now;
        if let Err(error) = self.save_index() {
            self.index = previous;
            return Err(format!("cannot save recipe index: {error}"));
        }
        Ok(())
    }

    /// Record a successful replay of `id` at one specific recipe version.
    ///
    /// Per-version health is updated and persisted; `entry.health` mirrors it
    /// only when `version` is the active version.
    pub fn record_success(
        &mut self,
        id: &str,
        version: u32,
        run_id: &str,
        now: u64,
    ) -> io::Result<()> {
        let entry = self.index_entry_mut(id, version, now);
        // v1 migration: a file without `versions` carries the active version's
        // health in `entry.health`; continue from it instead of starting blank.
        let seed = if entry.active_version == version {
            entry.health.clone()
        } else {
            RecipeHealth::default()
        };
        let mirrored = {
            let health = entry.versions.entry(version).or_insert(seed);
            health.record_success(run_id, now);
            health.clone()
        };
        if entry.active_version == version {
            entry.health = mirrored;
        }
        entry.updated_ms = now;
        self.index.updated_ms = now;
        self.save_index()
    }

    /// Record a failed replay of `id` at one specific recipe version
    /// (kind-aware, see [`RecipeHealth`]).
    pub fn record_failure(
        &mut self,
        id: &str,
        version: u32,
        run_id: &str,
        kind: ResolveErrorKind,
        step_id: Option<&str>,
        now: u64,
    ) -> io::Result<()> {
        let entry = self.index_entry_mut(id, version, now);
        let seed = if entry.active_version == version {
            entry.health.clone()
        } else {
            RecipeHealth::default()
        };
        let mirrored = {
            let health = entry.versions.entry(version).or_insert(seed);
            health.record_failure(run_id, &kind, step_id, now);
            health.clone()
        };
        if entry.active_version == version {
            entry.health = mirrored;
        }
        entry.updated_ms = now;
        self.index.updated_ms = now;
        self.save_index()
    }

    /// Fetch (or lazily create) the index entry used by `record_*`; a missing
    /// entry starts with `active_version = newest registered version`.
    fn index_entry_mut(&mut self, id: &str, version: u32, now: u64) -> &mut RecipeIndexEntry {
        let newest = self.latest_version(id).unwrap_or(version);
        self.index
            .entries
            .entry(id.to_owned())
            .or_insert_with(|| RecipeIndexEntry {
                active_version: newest,
                updated_ms: now,
                ..RecipeIndexEntry::default()
            })
    }

    /// Atomically persist the index (`<root>/../index.json` or
    /// `<root>/index.json`; see the module docs).
    pub fn save_index(&self) -> io::Result<()> {
        let root = self
            .root
            .as_ref()
            .ok_or_else(|| io::Error::other("recipe registry has no root directory"))?;
        let path = index_path(root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(&self.index)
            .map_err(|error| io::Error::other(format!("cannot serialize recipe index: {error}")))?;
        write_atomic(&path, &text)
    }

    fn latest_version(&self, id: &str) -> Option<u32> {
        self.recipes
            .iter()
            .filter(|recipe| recipe.id == id)
            .map(|recipe| recipe.version)
            .max()
    }

    fn is_active(&self, recipe: &Recipe) -> bool {
        match self.index.entries.get(&recipe.id) {
            Some(entry) => entry.active_version == recipe.version,
            None => self.latest_version(&recipe.id) == Some(recipe.version),
        }
    }
}

/// Location of `index.json` for a given *recipes* root.
pub fn index_path(root: &Path) -> PathBuf {
    let is_recipes_dir = root.file_name().and_then(|name| name.to_str()) == Some("recipes");
    if is_recipes_dir {
        if let Some(parent) = root.parent() {
            return parent.join("index.json");
        }
    }
    root.join("index.json")
}

/// Tolerant index load: missing or corrupt files yield an empty index.
fn load_index(root: &Path) -> RecipeIndex {
    let path = index_path(root);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "rdm-resolve: cannot read recipe index; starting empty"
                );
            }
            return RecipeIndex::default();
        }
    };
    match serde_json::from_str::<RecipeIndex>(&text) {
        Ok(mut index) => {
            // Always expose the current schema in memory: a loaded v1 file keeps
            // its old `health` (treated as the active version's health) and is
            // upgraded to v2 on the next save, without rewriting it on load.
            index.schema = INDEX_SCHEMA.to_owned();
            index
        }
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "rdm-resolve: invalid recipe index; starting empty"
            );
            RecipeIndex::default()
        }
    }
}

/// `<safe-id>@<version>.json`
pub fn recipe_file_name(recipe: &Recipe) -> String {
    let safe: String = recipe
        .id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    format!("{safe}@{}.json", recipe.version)
}

fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("recipe");
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!(".{name}.{}.{sequence}.tmp", std::process::id()));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::ResolveErrorKind;
    use crate::test_support::TempDir;

    fn recipe(id: &str, version: u32, pattern: &str, priority: i32) -> Recipe {
        serde_json::from_value(serde_json::json!({
            "schema": "rdm.recipe/v1",
            "id": id,
            "version": version,
            "match": { "hosts": ["example.com"], "url_pattern": pattern, "priority": priority },
            "steps": [{ "id": "s", "op": "assert", "args": { "kind": "contains", "value": "x", "needle": "x" } }],
            "emit": { "url": "https://cdn.example.com/f.bin" }
        }))
        .unwrap()
    }

    fn indexed_recipe(id: &str, version: u32) -> Recipe {
        recipe(id, version, "^https://example\\.com/", 0)
    }

    #[test]
    fn picks_higher_priority_then_specificity() {
        let mut registry = RecipeRegistry::empty();
        registry
            .insert(recipe("loose", 1, "^https://example\\.com/", 0))
            .unwrap();
        registry
            .insert(recipe("tight", 1, "^https://example\\.com/d/\\d+$", 0))
            .unwrap();
        registry
            .insert(recipe("priority", 1, "^https://example\\.com/", 5))
            .unwrap();
        let url = Url::parse("https://example.com/d/1").unwrap();
        let hit = registry.find_for_url(&url).unwrap();
        assert_eq!(hit.recipe.id, "priority");
    }

    #[test]
    fn replaces_same_id_and_version() {
        let mut registry = RecipeRegistry::empty();
        registry
            .insert(recipe("a", 1, "^https://example\\.com/", 0))
            .unwrap();
        registry
            .insert(recipe("a", 1, "^https://example\\.com/x", 0))
            .unwrap();
        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry
                .get("a", None)
                .unwrap()
                .matcher
                .url_pattern
                .as_deref(),
            Some("^https://example\\.com/x")
        );
    }

    #[test]
    fn get_without_version_returns_newest() {
        let mut registry = RecipeRegistry::empty();
        registry
            .insert(recipe("a", 1, "^https://example\\.com/", 0))
            .unwrap();
        registry
            .insert(recipe("a", 2, "^https://example\\.com/", 0))
            .unwrap();
        assert_eq!(registry.get("a", None).unwrap().version, 2);
        assert_eq!(registry.get("a", Some(1)).unwrap().version, 1);
    }

    #[test]
    fn saves_and_reloads_recipe() {
        let dir = TempDir::new("registry-save");
        let registry = RecipeRegistry::with_root(dir.path());
        let recipe = recipe("example.com-file", 3, "^https://example\\.com/d/\\d+$", 0);
        let path = registry.save(&recipe).unwrap();
        assert!(path.exists());
        let reloaded = RecipeRegistry::load(dir.path());
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded.get("example.com-file", None).unwrap().version, 3);
    }

    #[test]
    fn file_name_is_sanitized() {
        let mut recipe = recipe("example.com/file", 2, "^https://example\\.com/", 0);
        recipe.id = "example.com/download:file".to_owned();
        assert_eq!(
            recipe_file_name(&recipe),
            "example.com_download_file@2.json"
        );
    }

    // ------------------------------------------------------------------
    // Index / active_version / health (P3a)
    // ------------------------------------------------------------------

    #[test]
    fn index_path_follows_the_documented_state_layout() {
        let state = Path::new("/state/resolve/recipes");
        assert_eq!(index_path(state), Path::new("/state/resolve/index.json"));
        let custom = Path::new("/tmp/whatever");
        assert_eq!(index_path(custom), Path::new("/tmp/whatever/index.json"));
    }

    #[test]
    fn missing_index_loads_empty_and_unknown_health_is_untested() {
        let dir = TempDir::new("registry-missing-index");
        let mut registry = RecipeRegistry::with_root(dir.path());
        let recipe = indexed_recipe("a", 1);
        registry.insert(recipe.clone()).unwrap();
        registry.save(&recipe).unwrap();
        assert_eq!(registry.active_version("a"), Some(1));
        assert_eq!(
            registry.health("a").status,
            crate::health::MethodStatus::Untested
        );
        assert_eq!(
            registry.health("nope").status,
            crate::health::MethodStatus::Untested
        );

        let reloaded = RecipeRegistry::load(dir.path());
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded.active_version("a"), None);
    }

    #[test]
    fn insert_promotes_active_version_to_the_newest_and_never_regresses() {
        let mut registry = RecipeRegistry::empty();
        registry.insert(indexed_recipe("a", 1)).unwrap();
        assert_eq!(registry.active_version("a"), Some(1));
        registry.insert(indexed_recipe("a", 2)).unwrap();
        assert_eq!(registry.active_version("a"), Some(2));
        registry.insert(indexed_recipe("a", 1)).unwrap();
        assert_eq!(
            registry.active_version("a"),
            Some(2),
            "insert never regresses active_version: max(existing, version)"
        );
        // Re-inserting a higher version promotes again.
        registry.insert(indexed_recipe("a", 3)).unwrap();
        assert_eq!(registry.active_version("a"), Some(3));
    }

    #[test]
    fn matcher_uses_only_the_active_version_and_falls_back_to_newest() {
        let mut registry = RecipeRegistry::empty();
        registry.insert(indexed_recipe("a", 1)).unwrap();
        registry.insert(indexed_recipe("a", 2)).unwrap();
        let url = Url::parse("https://example.com/d/1").unwrap();
        assert_eq!(registry.find_for_url(&url).unwrap().recipe.version, 2);

        // Without any index entry the newest version still wins.
        let fallback = RecipeRegistry {
            recipes: registry.recipes.clone(),
            root: None,
            index: RecipeIndex::default(),
        };
        assert_eq!(fallback.find_for_url(&url).unwrap().recipe.version, 2);

        // In-memory index rollback (no root → save fails, so use a rooted one).
        let dir = TempDir::new("registry-active");
        let mut rooted = RecipeRegistry::with_root(dir.path());
        for recipe in registry.recipes.clone() {
            rooted.save(&recipe).unwrap();
            rooted.insert(recipe).unwrap();
        }
        rooted.set_active_version("a", 1).unwrap();
        assert_eq!(rooted.find_for_url(&url).unwrap().recipe.version, 1);
        assert_eq!(
            rooted.get("a", None).unwrap().version,
            2,
            "get(_, None) keeps returning the newest version"
        );
        // Persisted index is reloaded.
        let reloaded = RecipeRegistry::load(dir.path());
        assert_eq!(reloaded.active_version("a"), Some(1));
        assert_eq!(reloaded.find_for_url(&url).unwrap().recipe.version, 1);
    }

    #[test]
    fn set_active_version_rejects_unknown_versions() {
        let dir = TempDir::new("registry-rollback-invalid");
        let mut registry = RecipeRegistry::with_root(dir.path());
        registry.insert(indexed_recipe("a", 1)).unwrap();
        let error = registry.set_active_version("a", 9).unwrap_err();
        assert!(error.contains("does not exist"), "{error}");
        assert!(registry.set_active_version("missing", 1).is_err());

        registry.save_index().unwrap();
        assert!(index_path(dir.path()).exists());
        let raw = std::fs::read_to_string(index_path(dir.path())).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(json["schema"], INDEX_SCHEMA);
        assert_eq!(json["entries"]["a"]["active_version"], 1);
    }

    #[test]
    fn health_is_persisted_in_the_index_and_survives_reload() {
        let dir = TempDir::new("registry-health");
        let mut registry = RecipeRegistry::with_root(dir.path());
        registry.insert(indexed_recipe("a", 2)).unwrap();
        registry.record_success("a", 2, "run_ok", 100).unwrap();
        registry
            .record_failure(
                "a",
                2,
                "run_bad",
                ResolveErrorKind::RecipeStale,
                Some("url"),
                200,
            )
            .unwrap();
        let health = registry.health("a");
        assert_eq!(
            health.status,
            crate::health::MethodStatus::Ok,
            "a success keeps `ok`; the first structural failure only counts"
        );
        assert_eq!(health.consecutive_failures, 1);
        assert_eq!(health.last_ok_ms, Some(100));
        assert_eq!(
            health
                .last_failure
                .as_ref()
                .map(|failure| failure.run_id.as_str()),
            Some("run_bad")
        );

        let reloaded = RecipeRegistry::load(dir.path());
        assert_eq!(reloaded.health("a"), health);
        let raw = std::fs::read_to_string(index_path(dir.path())).unwrap();
        assert!(raw.contains("\"recipe_stale\""), "{raw}");
    }

    #[test]
    fn corrupt_index_is_ignored_but_recipes_still_load() {
        let dir = TempDir::new("registry-corrupt-index");
        let mut registry = RecipeRegistry::with_root(dir.path());
        registry.insert(indexed_recipe("a", 1)).unwrap();
        registry.save(registry.get("a", Some(1)).unwrap()).unwrap();
        std::fs::write(index_path(dir.path()), "{ not json").unwrap();

        let reloaded = RecipeRegistry::load(dir.path());
        assert_eq!(reloaded.len(), 1);
        assert!(reloaded.index().entries.is_empty());
        assert_eq!(reloaded.active_version("a"), None);
        // Matching still falls back to the newest version.
        let url = Url::parse("https://example.com/x").unwrap();
        assert_eq!(reloaded.find_for_url(&url).unwrap().recipe.version, 1);
    }

    #[test]
    fn save_index_without_root_is_an_error() {
        let registry = RecipeRegistry::empty();
        let error = registry.save_index().expect_err("no root");
        assert!(error.to_string().contains("no root"));
        let mut registry = RecipeRegistry::empty();
        registry.insert(indexed_recipe("a", 1)).unwrap();
        assert!(registry.record_success("a", 1, "run_1", 1).is_err());
    }

    #[test]
    fn old_index_json_without_optional_fields_loads() {
        let dir = TempDir::new("registry-old-index");
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            index_path(dir.path()),
            r#"{"schema":"rdm.recipe-index/v1","entries":{"a":{"active_version":3}}}"#,
        )
        .unwrap();
        let registry = RecipeRegistry::load(dir.path());
        assert_eq!(registry.active_version("a"), Some(3));
        assert_eq!(
            registry.health("a").status,
            crate::health::MethodStatus::Untested
        );
    }

    // ------------------------------------------------------------------
    // Version-level health (v0.15, index v2)
    // ------------------------------------------------------------------

    fn structural_failure(registry: &mut RecipeRegistry, id: &str, version: u32, run: &str) {
        registry
            .record_failure(
                id,
                version,
                run,
                ResolveErrorKind::RecipeStale,
                Some("step"),
                1,
            )
            .unwrap();
    }

    #[test]
    fn per_version_health_is_isolated_and_mirrors_the_active_version() {
        let dir = TempDir::new("registry-version-health");
        let mut registry = RecipeRegistry::with_root(dir.path());
        registry.insert(indexed_recipe("a", 1)).unwrap();
        structural_failure(&mut registry, "a", 1, "run_1");
        structural_failure(&mut registry, "a", 1, "run_2");
        assert_eq!(
            registry.health_for("a", 1).status,
            crate::health::MethodStatus::Degraded
        );
        assert_eq!(registry.health_for("a", 1).consecutive_failures, 2);

        // A new version starts clean; the old version's history is preserved.
        registry.insert(indexed_recipe("a", 2)).unwrap();
        assert_eq!(registry.active_version("a"), Some(2));
        assert_eq!(
            registry.health_for("a", 1).status,
            crate::health::MethodStatus::Degraded
        );
        assert_eq!(
            registry.health_for("a", 2).status,
            crate::health::MethodStatus::Untested
        );
        assert_eq!(
            registry.health("a").status,
            crate::health::MethodStatus::Untested,
            "entry.health mirrors the new active version"
        );

        registry.record_success("a", 2, "run_ok", 100).unwrap();
        registry.record_success("a", 2, "run_ok_2", 101).unwrap();
        assert_eq!(
            registry.health_for("a", 2).status,
            crate::health::MethodStatus::Ok
        );
        assert_eq!(
            registry.health_for("a", 1).status,
            crate::health::MethodStatus::Degraded,
            "other versions are unaffected"
        );
        assert_eq!(registry.health("a"), registry.health_for("a", 2));
        let versions = registry.health_by_version("a").expect("entry");
        assert_eq!(versions.len(), 2, "{versions:?}");
    }

    #[test]
    fn recording_a_non_active_version_does_not_move_the_mirror() {
        let dir = TempDir::new("registry-version-nonactive");
        let mut registry = RecipeRegistry::with_root(dir.path());
        registry.insert(indexed_recipe("a", 1)).unwrap();
        registry.insert(indexed_recipe("a", 2)).unwrap();
        registry.record_success("a", 2, "run_ok", 100).unwrap();
        assert_eq!(registry.health("a").status, crate::health::MethodStatus::Ok);

        // Three structural failures of the *inactive* v1 leave the v2 mirror ok.
        for index in 0..3 {
            structural_failure(&mut registry, "a", 1, &format!("run_bad_{index}"));
        }
        assert_eq!(
            registry.health_for("a", 1).status,
            crate::health::MethodStatus::Broken
        );
        assert_eq!(
            registry.health("a").status,
            crate::health::MethodStatus::Ok,
            "entry.health always mirrors the active version"
        );
    }

    #[test]
    fn rollback_mirrors_the_target_version_health() {
        let dir = TempDir::new("registry-version-rollback");
        let mut registry = RecipeRegistry::with_root(dir.path());
        registry.insert(indexed_recipe("a", 1)).unwrap();
        structural_failure(&mut registry, "a", 1, "run_1");
        structural_failure(&mut registry, "a", 1, "run_2");
        registry.insert(indexed_recipe("a", 2)).unwrap();
        registry.record_success("a", 2, "run_ok", 100).unwrap();

        registry.set_active_version("a", 1).unwrap();
        assert_eq!(registry.active_version("a"), Some(1));
        assert_eq!(
            registry.health_for("a", 1).status,
            crate::health::MethodStatus::Degraded
        );
        assert_eq!(
            registry.health("a").status,
            crate::health::MethodStatus::Degraded
        );

        registry.set_active_version("a", 2).unwrap();
        assert_eq!(registry.health("a").status, crate::health::MethodStatus::Ok);
        assert_eq!(
            registry.health_for("a", 1).status,
            crate::health::MethodStatus::Degraded,
            "history survives rollbacks"
        );

        let reloaded = RecipeRegistry::load(dir.path());
        assert_eq!(reloaded.health("a"), registry.health("a"));
        assert_eq!(reloaded.health_for("a", 1), registry.health_for("a", 1));
    }

    #[test]
    fn rollback_to_a_version_without_history_is_untested() {
        let dir = TempDir::new("registry-version-rollback-untested");
        let mut registry = RecipeRegistry::with_root(dir.path());
        registry.insert(indexed_recipe("a", 1)).unwrap();
        registry.insert(indexed_recipe("a", 2)).unwrap();
        registry.record_success("a", 2, "run_ok", 100).unwrap();
        // v1 was never replayed: rolling back gives a clean slate, but the old
        // active (v2) history is solidified first.
        registry.set_active_version("a", 1).unwrap();
        assert_eq!(
            registry.health("a").status,
            crate::health::MethodStatus::Untested
        );
        assert_eq!(
            registry.health_for("a", 2).status,
            crate::health::MethodStatus::Ok,
            "the previously mirrored health stays as v2 history"
        );
    }

    #[test]
    fn reinserting_the_same_version_does_not_reset_its_health() {
        let dir = TempDir::new("registry-version-reinsert");
        let mut registry = RecipeRegistry::with_root(dir.path());
        registry.insert(indexed_recipe("a", 2)).unwrap();
        registry.record_success("a", 2, "run_ok", 100).unwrap();
        registry.insert(indexed_recipe("a", 2)).unwrap();
        assert_eq!(
            registry.health("a").status,
            crate::health::MethodStatus::Ok,
            "same-version insert must not reset history"
        );
        assert_eq!(registry.active_version("a"), Some(2));
    }

    #[test]
    fn v1_index_is_migrated_in_memory_and_written_as_v2() {
        let dir = TempDir::new("registry-version-v1-migration");
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            index_path(dir.path()),
            r#"{
              "schema": "rdm.recipe-index/v1",
              "entries": {
                "a": {
                  "active_version": 2,
                  "health": { "status": "broken", "consecutive_failures": 3 }
                }
              }
            }"#,
        )
        .unwrap();
        // Loading does not rewrite the file…
        let before = std::fs::read_to_string(index_path(dir.path())).unwrap();
        assert!(before.contains("rdm.recipe-index/v1"));
        let mut registry = RecipeRegistry::with_root(dir.path());
        assert_eq!(registry.index().schema, INDEX_SCHEMA);
        assert_eq!(
            registry.health_for("a", 2).status,
            crate::health::MethodStatus::Broken,
            "v1 health belongs to the active version"
        );
        assert_eq!(
            registry.health_for("a", 1).status,
            crate::health::MethodStatus::Untested
        );
        assert_eq!(
            registry.health("a").status,
            crate::health::MethodStatus::Broken
        );

        // …the next save upgrades it to v2 and keeps per-version history.
        registry.record_success("a", 2, "run_ok", 100).unwrap();
        let after = std::fs::read_to_string(index_path(dir.path())).unwrap();
        let json: serde_json::Value = serde_json::from_str(&after).unwrap();
        assert_eq!(json["schema"], INDEX_SCHEMA);
        assert_eq!(json["entries"]["a"]["versions"]["2"]["status"], "ok");
        let reloaded = RecipeRegistry::load(dir.path());
        assert_eq!(
            reloaded.health_for("a", 2).status,
            crate::health::MethodStatus::Ok
        );
        assert_eq!(
            reloaded.health_for("a", 1).status,
            crate::health::MethodStatus::Untested
        );
    }

    #[test]
    fn record_creates_a_missing_entry_for_the_replayed_version() {
        let dir = TempDir::new("registry-version-missing-entry");
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            dir.path().join(recipe_file_name(&indexed_recipe("a", 3))),
            serde_json::to_string(&indexed_recipe("a", 3)).unwrap(),
        )
        .unwrap();
        let mut registry = RecipeRegistry::load(dir.path());
        registry.root = Some(dir.path().to_path_buf());
        assert!(registry.index().entries.is_empty());

        structural_failure(&mut registry, "a", 3, "run_1");
        let entry = &registry.index().entries["a"];
        assert_eq!(entry.active_version, 3);
        assert_eq!(registry.health_for("a", 3).consecutive_failures, 1);
        assert_eq!(registry.health("a"), registry.health_for("a", 3));
        assert_eq!(registry.health_for("a", 1), RecipeHealth::default());
    }

    #[test]
    fn schema_is_v2_and_versions_json_keys_are_strings() {
        let dir = TempDir::new("registry-version-schema");
        let mut registry = RecipeRegistry::with_root(dir.path());
        registry.insert(indexed_recipe("a", 1)).unwrap();
        registry.record_success("a", 1, "run_ok", 100).unwrap();
        let raw = std::fs::read_to_string(index_path(dir.path())).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(json["schema"], "rdm.recipe-index/v2");
        assert_eq!(json["entries"]["a"]["versions"]["1"]["status"], "ok");
        assert_eq!(json["entries"]["a"]["health"]["status"], "ok");
    }
}

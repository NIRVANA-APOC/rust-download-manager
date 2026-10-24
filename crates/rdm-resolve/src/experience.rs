//! Resolve experience library (design 17 §2.1, design 18 §2.4).
//!
//! An *experience* is the user-facing projection of one recipe id accepted by
//! the model-free matcher: its active version, flow timeline, version history,
//! health, usage counters, resource richness, resource kinds and — since
//! experience 2.0 — the optional bundle of merged supplementary flows and the
//! same-class `related` experiences. This module owns the file and domain logic
//! only (no UI); GUI/CLI share it through [`ExperienceStore`].
//!
//! Facts come from the same on-disk layout as [`crate::registry`]:
//! `<state>/resolve/recipes/<safe-id>@<version>.json` plus
//! `<state>/resolve/index.json` (schema v4). Every write is explicit and
//! fallible (`Result<_, String>`, rendered inline by the GUI); nothing here
//! panics on I/O errors, and no request headers, bodies or cookies are ever
//! persisted.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::contract::now_ms;
use crate::draft;
use crate::health::RecipeHealth;
use crate::recipe::{Recipe, RecipeLimits, RecipeStep, StepWhen, DEFAULT_BOUNDED_EACH_MAX};
use crate::registry::{BundleRef, RecipeIndexEntry, RecipeRegistry};

/// File/domain store for the resolve experience library.
///
/// `resolve_root` is `<state>/resolve` (the directory that contains `recipes/`,
/// `drafts/`, `runs/` and `index.json`).
#[derive(Debug, Clone)]
pub struct ExperienceStore {
    resolve_root: PathBuf,
}

impl ExperienceStore {
    pub fn new(resolve_root: impl Into<PathBuf>) -> Self {
        Self {
            resolve_root: resolve_root.into(),
        }
    }

    /// `<state>/resolve`.
    pub fn from_state_dir(state_dir: &Path) -> Self {
        Self::new(state_dir.join("resolve"))
    }

    /// The `<state>/resolve` root this store reads/writes.
    pub fn root(&self) -> &Path {
        &self.resolve_root
    }

    fn registry(&self) -> RecipeRegistry {
        RecipeRegistry::with_root(self.resolve_root.join("recipes"))
    }

    /// Every visible experience, most recently updated first (ties by id).
    ///
    /// Experiences merged into another one are hidden (design 18 §2.4); use
    /// [`Self::get`] for their detail.
    pub fn list(&self) -> Vec<Experience> {
        let registry = self.registry();
        let mut experiences: Vec<Experience> = registry
            .visible_ids()
            .into_iter()
            .filter_map(|id| experience_for(&registry, &id))
            .collect();
        experiences.sort_by(|left, right| {
            right
                .updated_ms
                .cmp(&left.updated_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        experiences
    }

    /// One experience by id (`None` when no recipe file exists for it).
    pub fn get(&self, id: &str) -> Option<Experience> {
        experience_for(&self.registry(), id)
    }

    /// Save `recipe` as the next version of `id` and activate it.
    ///
    /// `recipe.id` is forced to `id` and `recipe.version` to `max_version + 1`;
    /// the old version files and their health stay untouched (rollback is
    /// possible via [`Self::set_active_version`]). The recipe is validated
    /// before any file is written.
    pub fn update(&self, id: &str, mut recipe: Recipe) -> Result<Experience, String> {
        let mut registry = self.registry();
        let Some(current) = registry.latest_version(id) else {
            return Err(format!("解析经验 {id:?} 不存在"));
        };
        let Some(next) = current.checked_add(1) else {
            return Err(format!("解析经验 {id:?} 版本号已用尽"));
        };
        recipe.id = id.to_owned();
        recipe.version = next;
        recipe.validate()?;
        let path = registry
            .save(&recipe)
            .map_err(|error| format!("无法保存解析经验文件：{error}"))?;
        if let Err(error) = registry.insert(recipe.clone()) {
            let _ = std::fs::remove_file(&path);
            return Err(format!("无法更新解析经验：{error}"));
        }
        // Record the new version's publish time (no richness information is
        // available for a manual edit, so the counters are left untouched).
        if let Err(error) = registry.note_publish(id, next, 0, None, now_ms(), false) {
            let _ = std::fs::remove_file(&path);
            return Err(format!("无法保存解析经验索引：{error}"));
        }
        experience_for(&registry, id).ok_or_else(|| format!("解析经验 {id:?} 不存在"))
    }

    /// Delete an experience **and its merged bundle** (design 18 §2.4):
    /// every version file, index entry and same-id draft of the main experience,
    /// of all bundled supplementary flows and of every experience that was
    /// merged into it. Dangling bundle references left on other experiences are
    /// cleaned best-effort. `Ok(false)` means the experience did not exist.
    pub fn delete(&self, id: &str) -> Result<bool, String> {
        let mut registry = self.registry();
        let exists = registry.entry(id).is_some() || registry.get(id, None).is_some();
        if !exists {
            return Ok(false);
        }
        // Walk the bundle graph both ways: bundled flows (target side) and
        // experiences merged into one of the removed ids (source side).
        let entries = registry.index().entries.clone();
        let mut removed: Vec<String> = vec![id.to_owned()];
        let mut queue: Vec<String> = vec![id.to_owned()];
        while let Some(current) = queue.pop() {
            if let Some(entry) = entries.get(&current) {
                for bundled in &entry.bundled {
                    if !removed.contains(&bundled.id) {
                        removed.push(bundled.id.clone());
                        queue.push(bundled.id.clone());
                    }
                }
            }
            for (candidate, entry) in &entries {
                if entry.merged_into.as_deref() == Some(current.as_str())
                    && !removed.contains(candidate)
                {
                    removed.push(candidate.clone());
                    queue.push(candidate.clone());
                }
            }
        }
        let mut removed_any = false;
        for candidate in &removed {
            match registry.remove(candidate) {
                Ok(removed_entry) => removed_any |= removed_entry,
                Err(error) => {
                    return Err(format!("无法删除解析经验 {candidate:?}：{error}"));
                }
            }
            if let Err(error) = draft::remove_draft(&self.resolve_root, candidate) {
                tracing::warn!(
                    id = %candidate,
                    error = %error,
                    "rdm-resolve: deleted experience but could not remove its drafts"
                );
            }
        }
        // Clean bundle references to deleted flows on every remaining entry.
        let dangling: Vec<String> = registry
            .index()
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry
                    .bundled
                    .iter()
                    .any(|bundled| removed.contains(&bundled.id))
            })
            .map(|(id, _)| id.clone())
            .collect();
        for candidate in dangling {
            let filtered: Vec<BundleRef> = registry
                .entry(&candidate)
                .map(|entry| {
                    entry
                        .bundled
                        .iter()
                        .filter(|bundled| !removed.contains(&bundled.id))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            if let Err(error) = registry.set_bundle(&candidate, &filtered) {
                tracing::warn!(
                    id = %candidate,
                    error = %error,
                    "rdm-resolve: could not clean dangling bundle references"
                );
            }
        }
        Ok(removed_any || exists)
    }

    /// Merge `sources` into `target` (design 18 §2.4).
    ///
    /// Only metadata is rewritten — no flow JSON is touched:
    /// * `target.bundled` += each source's active version + the source's own
    ///   bundled references (so nested merges are flattened);
    /// * every source gets `merged_into = Some(target)` and disappears from
    ///   [`Self::list`];
    /// * `target.best_resources` becomes `target + Σ source.best_resources`
    ///   and `target.resource_kinds` becomes the union.
    pub fn merge(&self, target: &str, sources: &[&str]) -> Result<Experience, String> {
        let mut registry = self.registry();
        let Some(target_entry) = registry.entry(target).cloned() else {
            return Err(format!("解析经验 {target:?} 不存在"));
        };
        if target_entry.merged_into.is_some() {
            return Err(format!(
                "解析经验 {target:?} 已被合并到 {:?}，不能再作为合并目标",
                target_entry.merged_into
            ));
        }
        if registry.get(target, None).is_none() {
            return Err(format!("解析经验 {target:?} 不存在"));
        }
        let mut unique_sources: Vec<String> = Vec::new();
        for source in sources {
            let source = source.trim();
            if source.is_empty() || source == target {
                continue;
            }
            if unique_sources.iter().any(|existing| existing == source) {
                continue;
            }
            let Some(entry) = registry.entry(source).cloned() else {
                return Err(format!("解析经验 {source:?} 不存在"));
            };
            if entry.merged_into.is_some() {
                return Err(format!(
                    "解析经验 {source:?} 已被合并到 {:?}",
                    entry.merged_into
                ));
            }
            if registry.get(source, None).is_none() {
                return Err(format!("解析经验 {source:?} 不存在"));
            }
            unique_sources.push(source.to_owned());
        }
        if unique_sources.is_empty() {
            return Err("没有可合并的同类经验".to_owned());
        }

        let mut bundled = target_entry.bundled.clone();
        let mut kinds = target_entry.resource_kinds.clone();
        let mut best = target_entry.best_resources;
        for source in &unique_sources {
            let entry: RecipeIndexEntry = registry.entry(source).cloned().unwrap_or_default();
            let version = if entry.active_version != 0 {
                entry.active_version
            } else {
                registry.latest_version(source).unwrap_or(0)
            };
            if version == 0 {
                return Err(format!("解析经验 {source:?} 没有可用版本"));
            }
            bundled.push(BundleRef {
                id: source.clone(),
                version,
            });
            bundled.extend(entry.bundled.iter().cloned());
            kinds.extend(entry.resource_kinds.iter().cloned());
            best = best.saturating_add(entry.best_resources);
        }
        registry
            .set_bundle(target, &bundled)
            .map_err(|error| format!("无法保存合并后的补充流程：{error}"))?;
        registry
            .set_resource_kinds(target, &kinds)
            .map_err(|error| format!("无法保存合并后的资源种类：{error}"))?;
        registry
            .set_richness(target, best, None)
            .map_err(|error| format!("无法保存合并后的资源数：{error}"))?;
        for source in &unique_sources {
            registry
                .set_merged_into(source, Some(target))
                .map_err(|error| format!("无法标记 {source:?} 为已合并：{error}"))?;
        }
        experience_for(&registry, target).ok_or_else(|| format!("解析经验 {target:?} 不存在"))
    }

    /// Move the active pointer to an existing version (rollback / pin).
    /// Persisted immediately; health history and recipe files are untouched.
    pub fn set_active_version(&self, id: &str, version: u32) -> Result<Experience, String> {
        let mut registry = self.registry();
        registry.set_active_version(id, version)?;
        experience_for(&registry, id).ok_or_else(|| format!("解析经验 {id:?} 不存在"))
    }
}

/// One user-facing resolution experience (design 17 §2.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Experience {
    pub id: String,
    pub name: String,
    pub description: String,
    pub hosts: Vec<String>,
    pub url_pattern: Option<String>,
    pub priority: i32,
    pub active_version: u32,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub use_count: u64,
    pub success_count: u64,
    pub failure_count: u64,
    pub last_used_ms: Option<u64>,
    pub best_resources: u32,
    pub last_resources: u32,
    pub source_url: Option<String>,
    /// Health of the active version.
    pub health: RecipeHealth,
    /// Flow of the active version.
    pub flow: ExperienceFlow,
    /// Every stored version, oldest first.
    pub versions: Vec<ExperienceVersion>,
    pub tests: Vec<crate::recipe::RecipeTest>,
    /// Full recipe of the active version (edit-dialog initial value).
    pub recipe: Recipe,
    /// Resource kind set (`["image"]`, `["video"]`, …; design 18 §2.4).
    pub resource_kinds: Vec<String>,
    /// `Some(target)` when this experience was merged into another one.
    pub merged_into: Option<String>,
    /// Main flow (first) plus every bundled supplementary flow.
    pub flows: Vec<ExperienceFlowView>,
    /// Same-class visible experiences that are not merged yet (merge hint),
    /// sorted by `created_ms` ascending.
    pub related: Vec<String>,
}

/// One flow of an experience (main flow or a merged supplementary flow).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperienceFlowView {
    pub recipe_id: String,
    pub version: u32,
    pub primary: bool,
    /// Recipe name, or `主流程` / `补充流程 N` fallbacks.
    pub name: String,
    pub flow: ExperienceFlow,
    pub recipe: Recipe,
    /// Known resource count of this flow (`0` = unknown).
    pub resources: u32,
}

impl Experience {
    /// List/detail title: the user name, else `host + path` from the first
    /// sample URL, else the first host, else the id.
    pub fn display_name(&self) -> String {
        let name = self.name.trim();
        if !name.is_empty() {
            return name.to_owned();
        }
        if let Some(parsed) = self
            .source_url
            .as_deref()
            .and_then(|url| Url::parse(url).ok())
        {
            let host = parsed.host_str().unwrap_or_default();
            let path = parsed.path().trim_end_matches('/');
            match (host.is_empty(), path.is_empty()) {
                (false, false) => return format!("{host}{path}"),
                (false, true) => return host.to_owned(),
                (true, false) => return path.to_owned(),
                (true, true) => {}
            }
        }
        if let Some(host) = self.hosts.first().filter(|host| !host.trim().is_empty()) {
            return host.trim().to_owned();
        }
        self.id.clone()
    }

    /// Scope line such as `example.com · 精确地址` (design 17 §2.1).
    pub fn scope_label(&self) -> String {
        let hosts = if self.hosts.is_empty() {
            "任意主机".to_owned()
        } else {
            self.hosts
                .iter()
                .map(|host| host.trim())
                .filter(|host| !host.is_empty())
                .collect::<Vec<_>>()
                .join("、")
        };
        let rule = match self
            .url_pattern
            .as_deref()
            .map(str::trim)
            .filter(|pattern| !pattern.is_empty())
        {
            None => "任意路径",
            Some(pattern) if pattern.starts_with('^') && pattern.ends_with('$') => "精确地址",
            Some(_) => "路径规则",
        };
        let hosts = if hosts.is_empty() {
            "任意主机".to_owned()
        } else {
            hosts
        };
        format!("{hosts} · {rule}")
    }
}

/// One stored version of an experience (rollback/edit preview).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperienceVersion {
    pub version: u32,
    pub active: bool,
    pub health: RecipeHealth,
    /// `version_resources`, `0` = unknown.
    pub resources: u32,
    pub created_ms: Option<u64>,
    pub flow: ExperienceFlow,
    /// Full recipe of this version.
    pub recipe: Recipe,
}

/// Readable flow timeline of one recipe version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperienceFlow {
    pub steps: Vec<FlowStep>,
    pub emit: FlowEmit,
    pub limits: RecipeLimits,
    pub captures: Vec<String>,
}

/// One timeline entry: operator, human summary and the machine-relevant rails.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowStep {
    pub index: usize,
    pub id: String,
    /// `http_request` / `json_query` / `regex_capture` / ...
    pub op: String,
    /// `GET https://… （≤1048576 B）`, `json_query /data/url`, ...
    pub summary: String,
    /// `["o0 ← body_json"]`
    pub assignments: Vec<String>,
    /// `["status ∈ [200, 206]"]`
    pub expects: Vec<String>,
    /// Readable `when` guard.
    pub condition: Option<String>,
    /// `on_fail` override (a `ResolveErrorKind` name).
    pub on_fail: Option<String>,
}

/// Emitted resource descriptor (`RecipeEmit` projection).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowEmit {
    pub url: String,
    pub name: Option<String>,
    pub kind: Option<String>,
    pub one_time: bool,
    pub single_stream: bool,
}

fn experience_for(registry: &RecipeRegistry, id: &str) -> Option<Experience> {
    let entry = registry.entry(id).cloned().unwrap_or_default();
    let active_version = if entry.active_version != 0 {
        entry.active_version
    } else {
        registry.latest_version(id)?
    };
    let recipe = registry
        .get(id, Some(active_version))
        .or_else(|| registry.get(id, None))?
        .clone();
    let mut versions: Vec<ExperienceVersion> = registry
        .all()
        .iter()
        .filter(|candidate| candidate.id == id)
        .map(|version_recipe| ExperienceVersion {
            version: version_recipe.version,
            active: version_recipe.version == active_version,
            health: registry.health_for(id, version_recipe.version),
            resources: entry
                .version_resources
                .get(&version_recipe.version)
                .copied()
                .unwrap_or(0),
            created_ms: entry
                .version_created_ms
                .get(&version_recipe.version)
                .copied()
                .filter(|created| *created != 0),
            flow: flow_for(version_recipe),
            recipe: version_recipe.clone(),
        })
        .collect();
    versions.sort_by_key(|version| version.version);

    let source_url = entry.source_url.clone().or_else(|| first_test_url(&recipe));
    let created_ms = if entry.created_ms != 0 {
        entry.created_ms
    } else {
        entry.updated_ms
    };
    let flows = flow_views(registry, id, &recipe, active_version, &entry);
    let related = if entry.merged_into.is_none() {
        related_ids(registry, id, &scope_key(&recipe))
    } else {
        Vec::new()
    };
    Some(Experience {
        id: id.to_owned(),
        name: recipe.name.clone(),
        description: recipe.description.clone(),
        hosts: recipe.matcher.hosts.clone(),
        url_pattern: recipe.matcher.url_pattern.clone(),
        priority: recipe.matcher.priority,
        active_version,
        created_ms,
        updated_ms: entry.updated_ms,
        use_count: entry.use_count,
        success_count: entry.success_count,
        failure_count: entry.failure_count,
        last_used_ms: entry.last_used_ms,
        best_resources: entry.best_resources,
        last_resources: entry.last_resources,
        source_url,
        health: registry.health_for(id, active_version),
        flow: flow_for(&recipe),
        versions,
        tests: recipe.tests.clone(),
        recipe,
        resource_kinds: entry.resource_kinds.clone(),
        merged_into: entry.merged_into.clone(),
        flows,
        related,
    })
}

/// Main flow first, then every bundled supplementary flow (missing bundle
/// members are skipped; design 18 §2.4).
fn flow_views(
    registry: &RecipeRegistry,
    id: &str,
    main: &Recipe,
    main_version: u32,
    entry: &RecipeIndexEntry,
) -> Vec<ExperienceFlowView> {
    let main_name = main.name.trim();
    let mut flows = vec![ExperienceFlowView {
        recipe_id: id.to_owned(),
        version: main_version,
        primary: true,
        name: if main_name.is_empty() {
            "主流程".to_owned()
        } else {
            main_name.to_owned()
        },
        flow: flow_for(main),
        recipe: main.clone(),
        resources: known_resources(registry, id, main_version),
    }];
    let mut supplement_index = 0usize;
    for bundled in &entry.bundled {
        let Some(recipe) = registry.get(&bundled.id, Some(bundled.version)) else {
            continue;
        };
        supplement_index += 1;
        let name = recipe.name.trim();
        flows.push(ExperienceFlowView {
            recipe_id: bundled.id.clone(),
            version: bundled.version,
            primary: false,
            name: if name.is_empty() {
                format!("补充流程 {supplement_index}")
            } else {
                name.to_owned()
            },
            flow: flow_for(recipe),
            recipe: recipe.clone(),
            resources: known_resources(registry, &bundled.id, bundled.version),
        });
    }
    flows
}

/// Known resource count of one exact flow version (`0` = unknown).
fn known_resources(registry: &RecipeRegistry, id: &str, version: u32) -> u32 {
    registry
        .entry(id)
        .and_then(|entry| entry.version_resources.get(&version).copied())
        .unwrap_or(0)
}

/// Same-class grouping key: `generalization.scope`, else `hosts|url_pattern`.
fn scope_key(recipe: &Recipe) -> String {
    if let Some(scope) = recipe
        .generalization
        .as_ref()
        .map(|generalization| generalization.scope.trim())
        .filter(|scope| !scope.is_empty())
    {
        return scope.to_ascii_lowercase();
    }
    let mut hosts: Vec<String> = recipe
        .matcher
        .hosts
        .iter()
        .map(|host| host.trim().to_ascii_lowercase())
        .filter(|host| !host.is_empty())
        .collect();
    hosts.sort();
    hosts.dedup();
    format!(
        "{}|{}",
        hosts.join(","),
        recipe.matcher.url_pattern.as_deref().unwrap_or_default()
    )
}

/// Visible experiences sharing `scope`, excluding `id`, by `created_ms` asc.
fn related_ids(registry: &RecipeRegistry, id: &str, scope: &str) -> Vec<String> {
    let mut related: Vec<(u64, String)> = Vec::new();
    for candidate in registry.visible_ids() {
        if candidate == id {
            continue;
        }
        let Some(entry) = registry.entry(&candidate) else {
            continue;
        };
        let version = if entry.active_version != 0 {
            entry.active_version
        } else {
            registry.latest_version(&candidate).unwrap_or(0)
        };
        let Some(recipe) = registry
            .get(&candidate, Some(version))
            .or_else(|| registry.get(&candidate, None))
        else {
            continue;
        };
        if scope_key(recipe) == scope {
            related.push((entry.created_ms, candidate));
        }
    }
    related.sort();
    related
        .into_iter()
        .map(|(_, candidate)| candidate)
        .collect()
}

fn first_test_url(recipe: &Recipe) -> Option<String> {
    recipe
        .tests
        .first()
        .map(|test| test.input_url.trim())
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
}

/// Project one recipe into the readable flow timeline (no I/O, no panic).
fn flow_for(recipe: &Recipe) -> ExperienceFlow {
    ExperienceFlow {
        steps: recipe
            .steps
            .iter()
            .enumerate()
            .map(|(index, step)| flow_step(index, step))
            .collect(),
        emit: FlowEmit {
            url: recipe.emit.url.clone(),
            name: recipe.emit.name.clone(),
            kind: recipe.emit.kind.clone(),
            one_time: recipe.emit.one_time,
            single_stream: recipe.emit.single_stream,
        },
        limits: recipe.limits,
        captures: recipe.captures.clone(),
    }
}

fn flow_step(index: usize, step: &RecipeStep) -> FlowStep {
    FlowStep {
        index,
        id: step.id.clone(),
        op: step.op.clone(),
        summary: op_summary(step),
        assignments: step
            .assign
            .iter()
            .map(|(name, source)| format!("{name} ← {source}"))
            .collect(),
        expects: step_expects(step),
        condition: step.when.as_ref().map(condition_summary),
        on_fail: step.on_fail.clone(),
    }
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn op_summary(step: &RecipeStep) -> String {
    let args = &step.args;
    match step.op.as_str() {
        "http_request" => {
            let method = arg_str(args, "method").unwrap_or("GET");
            let url = arg_str(args, "url").unwrap_or_default();
            let max_bytes = args
                .get("max_bytes")
                .and_then(Value::as_u64)
                .unwrap_or(64 * 1024);
            format!("{method} {url} （≤{max_bytes} B）")
        }
        "json_query" => format!(
            "json_query {}",
            arg_str(args, "pointer").unwrap_or_default()
        ),
        "regex_capture" => {
            let pattern = arg_str(args, "pattern").unwrap_or_default();
            match args.get("group").and_then(Value::as_u64) {
                Some(group) => format!("regex_capture {pattern}（group {group}）"),
                None => format!("regex_capture {pattern}"),
            }
        }
        "url_join" => format!(
            "url_join {} + {}",
            arg_str(args, "base").unwrap_or_default(),
            arg_str(args, "relative").unwrap_or_default()
        ),
        "template" => format!("template {}", arg_str(args, "template").unwrap_or_default()),
        "assert" => assert_summary(args),
        "branch" => format!(
            "条件分支 {} {} {}",
            arg_str(args, "var").unwrap_or_default(),
            arg_str(args, "kind").unwrap_or_default(),
            arg_str(args, "expected").unwrap_or_default()
        )
        .trim_end()
        .to_owned(),
        "bounded_each" => format!(
            "遍历 {}（最多 {} 项）",
            arg_str(args, "from").unwrap_or_default(),
            args.get("max")
                .and_then(Value::as_u64)
                .unwrap_or(u64::from(DEFAULT_BOUNDED_EACH_MAX))
        ),
        other => other.to_owned(),
    }
}

fn assert_summary(args: &Value) -> String {
    let value = arg_str(args, "value").unwrap_or_default();
    match arg_str(args, "kind").unwrap_or_default() {
        "host_suffix" => format!(
            "校验主机后缀 {}",
            arg_str(args, "suffix").unwrap_or_default()
        ),
        "matches" => format!(
            "校验 {value} 匹配 {}",
            arg_str(args, "pattern").unwrap_or_default()
        ),
        "contains" => format!(
            "校验 {value} 包含 {}",
            arg_str(args, "needle").unwrap_or_default()
        ),
        "equals" => format!(
            "校验 {value} == {}",
            arg_str(args, "expected").unwrap_or_default()
        ),
        other => format!("校验 {other}"),
    }
}

fn step_expects(step: &RecipeStep) -> Vec<String> {
    let Some(expect) = &step.expect else {
        return Vec::new();
    };
    let mut expects = Vec::new();
    if !expect.status_in.is_empty() {
        let statuses: Vec<String> = expect.status_in.iter().map(u16::to_string).collect();
        expects.push(format!("status ∈ [{}]", statuses.join(", ")));
    }
    if !expect.content_type_prefix.is_empty() {
        expects.push(format!(
            "content-type ∈ [{}]",
            expect.content_type_prefix.join(", ")
        ));
    }
    expects
}

fn condition_summary(when: &StepWhen) -> String {
    let expected = when.expected.as_deref().unwrap_or_default();
    match when.kind.as_str() {
        "exists" => format!("当 {} 存在", when.var),
        "equals" => format!("当 {} == {expected}", when.var),
        "contains" => format!("当 {} 包含 {expected}", when.var),
        other => format!("当 {} {other} {expected}", when.var)
            .trim_end()
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe_json(name: &str) -> Recipe {
        Recipe::from_json(
            &serde_json::json!({
                "schema": "rdm.recipe/v1",
                "id": "site",
                "version": 1,
                "name": name,
                "match": {
                    "hosts": ["example.com"],
                    "url_pattern": "^https://example\\.com/x$",
                    "priority": 0
                },
                "steps": [
                    {
                        "id": "fetch",
                        "op": "http_request",
                        "args": { "method": "GET", "url": "https://example.com/api", "max_bytes": 1024 },
                        "expect": { "status_in": [200, 206], "content_type_prefix": ["application/json"] },
                        "assign": { "o0": "body_json" },
                        "when": { "kind": "exists", "var": "o0" }
                    },
                    {
                        "id": "pick",
                        "op": "json_query",
                        "args": { "from": "o0", "pointer": "/file/url" },
                        "assign": { "v0": "value" }
                    },
                    {
                        "id": "check",
                        "op": "assert",
                        "args": { "kind": "matches", "value": "{{v0}}", "pattern": "^https?://" },
                        "on_fail": "recipe_stale"
                    }
                ],
                "emit": { "url": "{{v0}}", "name": "file.bin" }
            })
            .to_string(),
        )
        .expect("valid recipe")
    }

    #[test]
    fn flow_summarizer_exposes_steps_assignments_expects_and_guard() {
        let recipe = recipe_json("");
        let flow = flow_for(&recipe);
        assert_eq!(flow.steps.len(), 3);
        assert_eq!(flow.steps[0].index, 0);
        assert_eq!(flow.steps[0].op, "http_request");
        assert!(flow.steps[0]
            .summary
            .contains("GET https://example.com/api"));
        assert_eq!(flow.steps[0].assignments, vec!["o0 ← body_json"]);
        assert_eq!(
            flow.steps[0].expects,
            vec![
                "status ∈ [200, 206]".to_owned(),
                "content-type ∈ [application/json]".to_owned()
            ]
        );
        assert_eq!(flow.steps[0].condition.as_deref(), Some("当 o0 存在"));
        assert_eq!(flow.steps[1].summary, "json_query /file/url");
        assert_eq!(flow.steps[1].assignments, vec!["v0 ← value"]);
        assert_eq!(flow.steps[2].summary, "校验 {{v0}} 匹配 ^https?://");
        assert_eq!(flow.steps[2].on_fail.as_deref(), Some("recipe_stale"));
        assert_eq!(flow.emit.name.as_deref(), Some("file.bin"));
        assert!(!flow.emit.one_time);
    }

    #[test]
    fn display_name_and_scope_label_fall_back() {
        let recipe = recipe_json("");
        let mut experience = Experience {
            id: "site".to_owned(),
            name: String::new(),
            description: String::new(),
            hosts: vec!["example.com".to_owned()],
            url_pattern: Some("^https://example\\.com/x$".to_owned()),
            priority: 0,
            active_version: 1,
            created_ms: 1,
            updated_ms: 1,
            use_count: 0,
            success_count: 0,
            failure_count: 0,
            last_used_ms: None,
            best_resources: 0,
            last_resources: 0,
            source_url: Some("https://example.com/downloads/page".to_owned()),
            health: RecipeHealth::default(),
            flow: flow_for(&recipe),
            versions: Vec::new(),
            tests: Vec::new(),
            recipe,
            resource_kinds: Vec::new(),
            merged_into: None,
            flows: Vec::new(),
            related: Vec::new(),
        };
        assert_eq!(experience.display_name(), "example.com/downloads/page");
        experience.source_url = Some("https://example.com/".to_owned());
        assert_eq!(experience.display_name(), "example.com");
        experience.source_url = None;
        assert_eq!(experience.display_name(), "example.com");
        assert_eq!(experience.scope_label(), "example.com · 精确地址");
        experience.url_pattern = Some("^https://example\\.com/".to_owned());
        assert_eq!(experience.scope_label(), "example.com · 路径规则");
        experience.url_pattern = None;
        assert_eq!(experience.scope_label(), "example.com · 任意路径");
    }
}

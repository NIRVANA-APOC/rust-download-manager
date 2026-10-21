//! Parse cache: parsed sources, results and short site notes
//! (`docs/design-next-features.md` §3.5).
//!
//! Stored at `<state_dir>/ai/parse-cache.json`; at most [`MAX_ENTRIES`] entries
//! survive (oldest `created_ms` evicted first). Never contains request
//! credentials: only URLs, resources, notes and a short instructions string.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::parse::{normalize_url, ParseReport, ParsedResource};
use crate::{truncate_chars, write_atomic};

/// Current cache schema version.
pub const CACHE_VERSION: u32 = 1;
/// Maximum cached parse results.
pub const MAX_ENTRIES: usize = 200;
/// Maximum hints returned per host (and retained per host).
pub const MAX_HOST_HINTS: usize = 5;
/// Longest cached instructions string.
const MAX_INSTRUCTIONS_CHARS: usize = 400;

fn default_version() -> u32 {
    CACHE_VERSION
}

/// One cached parse result (`key = normalize_url(source_url)`).
///
/// Every field is `#[serde(default)]`: a future field addition/removal must not
/// make `load` silently discard the whole cache (the previous schema had no
/// defaults, so any incompatible change wiped every entry).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedParse {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub source_kind: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub resources: Vec<ParsedResource>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub created_ms: u64,
    #[serde(default)]
    pub model: Option<String>,
    /// Prompt/site gist used when this result was produced (short, no secrets).
    #[serde(default)]
    pub instructions: String,
    /// Set when the entry is a degraded report (see `parse::failure_kind`);
    /// `None` for a normal successful parse.
    #[serde(default)]
    pub failure_kind: Option<String>,
}

/// On-disk parse cache.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParseCache {
    #[serde(default = "default_version")]
    pub version: u32,
    /// key = `normalize_url`
    #[serde(default)]
    pub entries: BTreeMap<String, CachedParse>,
    /// key = host
    #[serde(default)]
    pub host_notes: BTreeMap<String, Vec<String>>,
}

impl ParseCache {
    /// `<state_dir>/ai/parse-cache.json`.
    pub fn cache_path(state_dir: &Path) -> PathBuf {
        state_dir.join("ai").join("parse-cache.json")
    }

    /// Load the cache; a missing or corrupted file yields an empty cache.
    pub fn load(state_dir: &Path) -> Self {
        let path = Self::cache_path(state_dir);
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Persist the cache with an atomic write (owner-only on Unix), pruning to
    /// [`MAX_ENTRIES`] first.
    pub fn save(&self, state_dir: &Path) -> io::Result<()> {
        let mut snapshot = self.clone();
        snapshot.version = CACHE_VERSION;
        snapshot.prune();
        let text = serde_json::to_string_pretty(&snapshot)
            .map_err(|error| io::Error::other(format!("cannot serialize parse cache: {error}")))?;
        write_atomic(&Self::cache_path(state_dir), &text)
    }

    /// Lookup by (normalized) URL.
    pub fn get(&self, url: &str) -> Option<&CachedParse> {
        let key = normalize_url(url).unwrap_or_else(|| url.trim().to_owned());
        self.entries.get(&key).or_else(|| {
            if key == url {
                None
            } else {
                self.entries.get(url)
            }
        })
    }

    /// Insert/replace the cache entry for `report` and refresh per-host notes.
    pub fn put(&mut self, report: &ParseReport, instructions: &str) {
        let key = normalize_url(&report.source_url).unwrap_or_else(|| report.source_url.clone());
        let entry = CachedParse {
            url: report.source_url.clone(),
            source_kind: report.source_kind.clone(),
            title: report.title.clone(),
            resources: report.resources.clone(),
            notes: report.notes.clone(),
            created_ms: report.created_ms,
            model: report.model.clone(),
            instructions: truncate_chars(instructions.trim(), MAX_INSTRUCTIONS_CHARS),
            failure_kind: report.failure_kind.clone(),
        };
        self.entries.insert(key, entry);

        if let Some(host) = host_of(&report.source_url) {
            let notes = self.host_notes.entry(host).or_default();
            for note in &report.notes {
                let note = truncate_chars(note.trim(), 200);
                if !note.is_empty() && !notes.contains(&note) {
                    notes.push(note);
                }
            }
            if notes.len() > MAX_HOST_HINTS {
                let excess = notes.len() - MAX_HOST_HINTS;
                notes.drain(0..excess);
            }
        }

        self.prune();
    }

    /// Short site notes for the host of `url` (at most [`MAX_HOST_HINTS`]).
    pub fn host_hints(&self, url: &str) -> Vec<String> {
        let Some(host) = host_of(url) else {
            return Vec::new();
        };
        match self.host_notes.get(&host) {
            Some(notes) => {
                let skip = notes.len().saturating_sub(MAX_HOST_HINTS);
                notes[skip..].to_vec()
            }
            None => Vec::new(),
        }
    }

    /// Drop the oldest entries until at most [`MAX_ENTRIES`] remain.
    pub(crate) fn prune(&mut self) {
        if self.entries.len() <= MAX_ENTRIES {
            return;
        }
        let mut order: Vec<(u64, String)> = self
            .entries
            .iter()
            .map(|(key, entry)| (entry.created_ms, key.clone()))
            .collect();
        order.sort();
        let excess = order.len() - MAX_ENTRIES;
        for (_, key) in order.into_iter().take(excess) {
            self.entries.remove(&key);
        }
    }
}

/// Process-wide lock table keyed by state directory.
///
/// `parse_url` mutates a whole-file cache, so two parses that each run
/// `load → parse → save` would silently drop one set of updates and both would
/// rewrite the same file. A [`ParseSession`] holds this lock for the whole
/// sequence.
fn session_locks() -> &'static std::sync::Mutex<BTreeMap<PathBuf, Arc<AsyncMutex<()>>>> {
    static LOCKS: OnceLock<std::sync::Mutex<BTreeMap<PathBuf, Arc<AsyncMutex<()>>>>> =
        OnceLock::new();
    LOCKS.get_or_init(|| std::sync::Mutex::new(BTreeMap::new()))
}

/// An exclusive, loaded view of the on-disk cache.
///
/// [`ParseSession::open`] waits for other sessions on the same state directory,
/// so concurrent parses serialize instead of losing each other's writes.
/// Call [`ParseSession::save`] before dropping when the cache was mutated.
pub struct ParseSession {
    state_dir: PathBuf,
    cache: ParseCache,
    _guard: OwnedMutexGuard<()>,
}

impl ParseSession {
    /// Wait for exclusive access to `state_dir`'s cache, then load it.
    pub async fn open(state_dir: &Path) -> Self {
        let lock = {
            let mut locks = session_locks()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            locks
                .entry(state_dir.to_path_buf())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let guard = lock.lock_owned().await;
        let cache = ParseCache::load(state_dir);
        Self {
            state_dir: state_dir.to_path_buf(),
            cache,
            _guard: guard,
        }
    }

    /// Read-only access to the loaded cache.
    pub fn cache(&self) -> &ParseCache {
        &self.cache
    }

    /// Mutable access for [`crate::parse_url`].
    pub fn cache_mut(&mut self) -> &mut ParseCache {
        &mut self.cache
    }

    /// Persist the cache (atomic write, pruned to [`MAX_ENTRIES`]).
    pub fn save(&mut self) -> io::Result<()> {
        self.cache.save(&self.state_dir)
    }
}

fn host_of(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url.trim()).ok()?;
    parsed.host_str().map(|host| host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "rdm-ai-store-{}-{tag}-{}",
                std::process::id(),
                crate::now_ms()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn report(url: &str, created_ms: u64) -> ParseReport {
        ParseReport {
            source_url: url.to_owned(),
            source_kind: "html_page".to_owned(),
            title: Some("Example".to_owned()),
            resources: vec![ParsedResource {
                url: "https://example.com/file.mp4".to_owned(),
                name: "file.mp4".to_owned(),
                kind: "video".to_owned(),
                size_bytes: Some(123),
                content_type: Some("video/mp4".to_owned()),
                note: None,
            }],
            notes: vec!["需要 Referer".to_owned()],
            ai_used: true,
            model: Some("gpt-4o-mini".to_owned()),
            created_ms,
            cache_hit: false,
            failure_kind: None,
        }
    }

    #[test]
    fn cache_path_layout() {
        assert_eq!(
            ParseCache::cache_path(Path::new("C:/state")),
            PathBuf::from("C:/state")
                .join("ai")
                .join("parse-cache.json")
        );
    }

    #[test]
    fn save_load_roundtrip() {
        let state = TempDir::new("roundtrip");
        assert_eq!(ParseCache::load(&state.0).entries.len(), 0);

        let mut cache = ParseCache::default();
        cache.put(
            &report("https://example.com/page#section", 1000),
            "title=Example",
        );
        cache.save(&state.0).unwrap();

        let loaded = ParseCache::load(&state.0);
        assert_eq!(loaded.version, CACHE_VERSION);
        let entry = loaded.get("https://example.com/page").unwrap();
        assert_eq!(entry.url, "https://example.com/page#section");
        assert_eq!(entry.resources.len(), 1);
        assert_eq!(entry.instructions, "title=Example");
        assert_eq!(entry.model.as_deref(), Some("gpt-4o-mini"));
    }

    #[test]
    fn corrupted_cache_loads_empty() {
        let state = TempDir::new("corrupt");
        std::fs::create_dir_all(state.0.join("ai")).unwrap();
        std::fs::write(ParseCache::cache_path(&state.0), "{oops").unwrap();
        assert_eq!(ParseCache::load(&state.0).entries.len(), 0);
    }

    #[test]
    fn entries_are_evicted_oldest_first() {
        let mut cache = ParseCache::default();
        for index in 0..(MAX_ENTRIES + 10) {
            let url = format!("https://example.com/page/{index}");
            cache.put(&report(&url, index as u64), "x");
        }
        assert_eq!(cache.entries.len(), MAX_ENTRIES);
        assert!(cache.get("https://example.com/page/0").is_none());
        assert!(cache.get("https://example.com/page/9").is_none());
        assert!(cache.get("https://example.com/page/10").is_some());
        assert!(cache
            .get(&format!("https://example.com/page/{}", MAX_ENTRIES + 9))
            .is_some());
    }

    #[test]
    fn host_hints_are_limited_and_scoped() {
        let mut cache = ParseCache::default();
        for index in 0..10 {
            let mut report = report(&format!("https://example.com/page/{index}"), index as u64);
            report.notes = vec![format!("note-{index}")];
            cache.put(&report, "x");
        }
        let hints = cache.host_hints("https://example.com/other");
        assert_eq!(hints.len(), MAX_HOST_HINTS);
        assert_eq!(hints.last().unwrap(), "note-9");
        assert!(cache.host_hints("https://other.example.net/x").is_empty());
        assert!(cache.host_hints("not a url").is_empty());
    }

    #[test]
    fn get_accepts_raw_and_normalized_url() {
        let mut cache = ParseCache::default();
        cache.put(&report("https://example.com/a", 1), "x");
        assert!(cache.get("https://example.com/a").is_some());
        assert!(cache.get("https://example.com/a#frag").is_some());
        assert!(cache.get("https://example.com/b").is_none());
    }

    #[tokio::test]
    async fn session_persists_through_reopen() {
        let state = TempDir::new("session-persist");
        let mut first = ParseSession::open(&state.0).await;
        first
            .cache_mut()
            .put(&report("https://example.com/a", 1), "x");
        first.save().unwrap();
        drop(first);

        let reopened = ParseSession::open(&state.0).await;
        assert!(reopened.cache().get("https://example.com/a").is_some());
    }

    #[tokio::test]
    async fn session_blocks_concurrent_open() {
        // Two concurrent parses used to each load/save the whole file, so one
        // silently dropped the other's entry.
        let state = TempDir::new("session-lock");
        let first = ParseSession::open(&state.0).await;
        let opened = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = opened.clone();
        let path = state.0.clone();
        let waiter = tokio::spawn(async move {
            let _second = ParseSession::open(&path).await;
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !opened.load(std::sync::atomic::Ordering::SeqCst),
            "a second session must wait for the first to finish"
        );
        drop(first);
        waiter.await.unwrap();
        assert!(opened.load(std::sync::atomic::Ordering::SeqCst));
    }
}

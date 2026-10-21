//! Test-only helpers: unique temporary directories without extra dependencies.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A unique, self-cleaning temporary directory.
pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub(crate) fn new(label: &str) -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "rdm-resolve-test-{label}-{}-{}-{}",
            std::process::id(),
            crate::contract::now_ms(),
            sequence
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

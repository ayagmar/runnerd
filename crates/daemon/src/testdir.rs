//! Temporary directories for tests: under the system temp dir, unique per process and call, removed on drop. Never
//! the user's home or config.

#![allow(
    clippy::unwrap_used,
    reason = "a test helper: a temp dir that cannot be made fails the test"
)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

/// A fresh directory, removed on drop.
pub(crate) struct Dir(pub PathBuf);

impl Dir {
    pub(crate) fn new() -> Dir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("runnerd-test-daemon-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Dir(path)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

//! Temporary workspaces and contexts for the tests.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Allowlist, Ctx, Workspace};

static NEXT: AtomicU64 = AtomicU64::new(0);

pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "agent-tools-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("creates a temporary directory");
        let path = std::fs::canonicalize(&path).expect("canonicalizes a temporary directory");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn workspace(&self) -> Workspace {
        Workspace::open(&self.path).expect("the fixture is a workspace")
    }

    pub fn write(
        &self,
        relative: &str,
        content: &str,
    ) {
        self.write_bytes(relative, content.as_bytes());
    }

    pub fn write_bytes(
        &self,
        relative: &str,
        content: &[u8],
    ) {
        let path = self.path.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("creates the parent");
        }
        std::fs::write(path, content).expect("writes the fixture");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

pub fn path_var() -> OsString {
    std::env::var_os("PATH").unwrap_or_default()
}

pub fn ctx<'a>(
    workspace: &'a Workspace,
    allow: &'a Allowlist,
    home: &Path,
) -> Ctx<'a> {
    Ctx::with_env(
        workspace,
        allow,
        vec![
            ("PATH".to_string(), path_var()),
            ("HOME".to_string(), home.as_os_str().to_os_string()),
            ("TMPDIR".to_string(), std::env::temp_dir().into_os_string()),
            ("LANG".to_string(), OsString::from("C")),
        ],
    )
}

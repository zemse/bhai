//! Conversation privacy, shared by admission and the loop's persistence boundary.

use std::cell::RefCell;
use std::path::Path;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

#[derive(Debug, Default)]
pub struct Privacy {
    state: AtomicU8,
    epoch: AtomicU64,
    pub(crate) io: Mutex<()>,
    inputs: Mutex<Vec<String>>,
}

impl Privacy {
    pub fn active(&self) -> bool {
        self.state.load(Ordering::SeqCst) != 0
    }

    pub fn ready(&self) -> bool {
        matches!(self.state.load(Ordering::SeqCst), 0 | 2)
    }

    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    pub fn public_at<T>(&self, epoch: u64, write: impl FnOnce() -> Result<T>) -> Result<T> {
        self.public(|| {
            anyhow::ensure!(
                epoch == self.epoch(),
                "conversation changed before export completed"
            );
            write()
        })
    }

    pub fn begin(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.state.store(1, Ordering::SeqCst);
    }

    pub fn finish(&self, success: bool) {
        self.state
            .store(if success { 2 } else { 3 }, Ordering::SeqCst);
    }

    pub fn clear(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.inputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.state.store(0, Ordering::SeqCst);
    }

    pub fn track(&self, text: &str) {
        self.inputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(text.to_string());
    }

    pub fn inputs(&self) -> Vec<String> {
        self.inputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Serialize a public export with the transition that erases it.
    pub fn public<T>(&self, write: impl FnOnce() -> Result<T>) -> Result<T> {
        let _guard = self.io.lock().unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(
            !self.active(),
            "not available in private mode; use /clear first"
        );
        write()
    }
}

tokio::task_local! {
    pub(crate) static CURRENT: RefCell<Arc<Privacy>>;
}

pub fn active() -> bool {
    CURRENT
        .try_with(|state| state.borrow().active())
        .unwrap_or(false)
}

pub(crate) fn raw_logging_disabled(path: Option<&std::ffi::OsStr>) -> Result<()> {
    anyhow::ensure!(
        path.is_none(),
        "BHAI_DEBUG_SSE raw logging is configured. Private mode cannot selectively erase that potentially shared file. Stop raw logging, handle its existing contents yourself, then restart without BHAI_DEBUG_SSE; no raw log was deleted"
    );
    Ok(())
}

pub fn writable() -> std::io::Result<()> {
    if active() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "private mode does not save conversation data",
        ));
    }
    Ok(())
}

pub(crate) fn current() -> Arc<Privacy> {
    CURRENT.with(|state| Arc::clone(&state.borrow()))
}

pub(crate) fn set(state: Arc<Privacy>) {
    CURRENT.with(|current| *current.borrow_mut() = state);
}

pub fn remove_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("could not erase {}", path.display())),
    }
}

pub(crate) fn erase_state(cwd: &Path, id: &str) -> Result<()> {
    let dir = crate::sessions::state_dir(cwd, id);
    erase_directory(&dir)?;
    erase_exports(cwd, id)
}

pub(crate) fn erase_directory(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => {
            return Err(error).with_context(|| format!("could not erase {}", dir.display()));
        }
    }
    Ok(())
}

pub(crate) fn erase_exports(cwd: &Path, id: &str) -> Result<()> {
    // Debug exports live beside the checkout, outside the session directory.
    for entry in std::fs::read_dir(cwd)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("bhai-debug-") && name.ends_with(&format!("-{id}.md")) {
            remove_file(&entry.path())?;
        }
    }
    Ok(())
}

pub(crate) fn erase_history(
    home: Option<&Path>,
    inputs: &[String],
    sessions: &[&str],
) -> Result<()> {
    let Some(home) = home else {
        return Ok(());
    };
    let path = home.join(".config/bhai/history.jsonl");
    use std::io::{Read as _, Seek as _, Write as _};
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("could not open prompt history for erasure"),
    };
    file.lock()?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let mut kept = String::new();
    for line in text.lines() {
        let record: serde_json::Value =
            serde_json::from_str(line).context("could not parse prompt history for erasure")?;
        let owned = record["session"].as_str().map(|id| sessions.contains(&id));
        let erase = owned.unwrap_or_else(|| {
            record["text"]
                .as_str()
                .is_some_and(|text| inputs.iter().any(|input| input == text))
        });
        if !erase {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    // Erasure bypasses the normal write guard, which is already latched closed.
    file.set_len(0)?;
    file.rewind()?;
    file.write_all(kept.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
#[path = "privacy_tests.rs"]
mod tests;

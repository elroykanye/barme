//! Registered webhooks, held in a single JSON file. Low volume and read whole
//! when an event fires, so a flat list is simplest. Rewritten atomically on
//! every change.

use crate::{write_atomic, Result};
use barme_core::Webhook;
use std::path::{Path, PathBuf};

pub struct WebhookStore {
    path: PathBuf,
}

impl WebhookStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        Ok(WebhookStore {
            path: root.join("webhooks.json"),
        })
    }

    pub fn list(&self) -> Result<Vec<Webhook>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    /// Add a hook, or replace an existing one with the same id.
    pub fn put(&self, hook: &Webhook) -> Result<()> {
        let mut hooks = self.list()?;
        hooks.retain(|h| h.id != hook.id);
        hooks.push(hook.clone());
        write_atomic(&self.path, &serde_json::to_vec(&hooks)?)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let mut hooks = self.list()?;
        let before = hooks.len();
        hooks.retain(|h| h.id != id);
        if hooks.len() != before {
            write_atomic(&self.path, &serde_json::to_vec(&hooks)?)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, WebhookStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = WebhookStore::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn put_list_delete() {
        let (_d, s) = store();
        assert!(s.list().unwrap().is_empty());
        s.put(&Webhook {
            id: "1".into(),
            url: "http://x/hook".into(),
            events: vec!["write".into()],
        })
        .unwrap();
        assert_eq!(s.list().unwrap().len(), 1);
        // Same id replaces rather than duplicates.
        s.put(&Webhook {
            id: "1".into(),
            url: "http://y/hook".into(),
            events: vec![],
        })
        .unwrap();
        let hooks = s.list().unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].url, "http://y/hook");
        s.delete("1").unwrap();
        assert!(s.list().unwrap().is_empty());
    }
}

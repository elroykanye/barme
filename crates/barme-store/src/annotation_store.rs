//! Per-object annotations, one small JSON file per `pot/key`. An object with no
//! file yet reads back as the default (empty) annotation. Mirrors [`MetaStore`],
//! but keyed by pot *and* key rather than pot alone.

use crate::{write_atomic, Result};
use barme_core::Annotation;
use std::path::{Path, PathBuf};

pub struct AnnotationStore {
    root: PathBuf,
}

impl AnnotationStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        Ok(AnnotationStore { root })
    }

    pub fn get(&self, pot: &str, key: &str) -> Result<Annotation> {
        match std::fs::read(self.path(pot, key)) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Annotation::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn set(&self, pot: &str, key: &str, annotation: &Annotation) -> Result<()> {
        write_atomic(&self.path(pot, key), &serde_json::to_vec(annotation)?)
    }

    // Hex-encode both pot and key into one filename so arbitrary names can't
    // escape the directory or collide.
    fn path(&self, pot: &str, key: &str) -> PathBuf {
        self.root.join(format!(
            "{}_{}.json",
            hex::encode(pot.as_bytes()),
            hex::encode(key.as_bytes())
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, AnnotationStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = AnnotationStore::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn unset_object_reads_default() {
        let (_d, s) = store();
        let a = s.get("photos", "cat.jpg").unwrap();
        assert!(a.tags.is_empty());
        assert!(!a.favorite);
    }

    #[test]
    fn set_then_read_back() {
        let (_d, s) = store();
        let mut a = Annotation::default();
        a.note = "a good cat".into();
        a.favorite = true;
        s.set("photos", "cat.jpg", &a).unwrap();
        let got = s.get("photos", "cat.jpg").unwrap();
        assert_eq!(got.note, "a good cat");
        assert!(got.favorite);
        // A different key is unaffected.
        assert!(s.get("photos", "dog.jpg").unwrap().note.is_empty());
    }
}

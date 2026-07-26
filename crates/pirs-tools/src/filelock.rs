use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();

pub type FileMutationGuard = tokio::sync::OwnedMutexGuard<()>;

/// Collapse path aliases so concurrent `edit`/`write` on the same inode share one lock.
/// Existing files canonicalize; missing leaves canonicalize the parent and reattach the name.
fn lock_key(path: &Path) -> PathBuf {
    if let Ok(canon) = std::fs::canonicalize(path) {
        return canon;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(cp) = std::fs::canonicalize(parent) {
            return cp.join(name);
        }
    }
    path.to_path_buf()
}

pub async fn lock(path: &Path) -> FileMutationGuard {
    let key = lock_key(path);
    let arc = {
        let mut map = LOCKS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap();
        map.entry(key)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    arc.lock_owned().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn serializes_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        let g1 = lock(&path).await;
        let path2 = path.clone();
        let contender = tokio::spawn(async move {
            lock(&path2).await;
            true
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!contender.is_finished());
        drop(g1);
        assert!(contender.await.unwrap());
    }
}

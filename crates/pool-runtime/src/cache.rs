//! On-disk layout: `<root>/models/<sha256>/<file>`, `<root>/runtimes/<id>-<target>/`,
//! `<root>/downloads/<sha256>/<archive>`, `<root>/miner-id`.

use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

use pool_protocol::catalog::Weights;

use crate::{Error, Result};

#[derive(Clone, Debug)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    /// `root` overrides the OS cache directory + `ai-pool`.
    pub fn new(root: Option<PathBuf>) -> Result<Self> {
        let root = match root {
            Some(root) => root,
            None => directories::BaseDirs::new()
                .ok_or_else(|| Error::msg("cannot determine the OS cache directory; pass --cache-dir"))?
                .cache_dir()
                .join("ai-pool"),
        };
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn model_dir(&self, weights: &Weights) -> PathBuf {
        self.root.join("models").join(&weights.sha256)
    }

    pub fn model_path(&self, weights: &Weights) -> PathBuf {
        self.model_dir(weights).join(&weights.filename)
    }

    pub fn runtime_dir(&self, runtime_id: &str, target: &str) -> PathBuf {
        self.root.join("runtimes").join(format!("{runtime_id}-{target}"))
    }

    pub fn download_path(&self, sha256: &str, name: &str) -> PathBuf {
        self.root.join("downloads").join(sha256).join(name)
    }

    /// Persistent installation label sent in `Hello`.
    pub fn miner_id(&self) -> Result<String> {
        let path = self.root.join("miner-id");
        if let Ok(id) = fs::read_to_string(&path) {
            let id = id.trim();
            if !id.is_empty() {
                return Ok(id.to_string());
            }
        }
        let id = uuid::Uuid::new_v4().to_string();
        fs::write(&path, &id)?;
        Ok(id)
    }

    /// Shared lock held while a miner serves this model, so `cache remove`
    /// can refuse to delete weights in use.
    pub fn hold_model(&self, weights: &Weights) -> Result<File> {
        let file = self.model_lock(weights)?;
        file.lock_shared()?;
        Ok(file)
    }

    /// Removes a cached model unless a running miner holds it.
    pub fn remove_model(&self, weights: &Weights) -> Result<bool> {
        let dir = self.model_dir(weights);
        if !dir.exists() {
            return Ok(false);
        }
        let lock = self.model_lock(weights)?;
        if lock.try_lock().is_err() {
            return Err(Error::msg(format!("{} is in use by a running miner", weights.filename)));
        }
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.file_name().is_some_and(|name| name != ".in-use.lock") {
                fs::remove_file(path)?;
            }
        }
        drop(lock);
        let _ = fs::remove_file(dir.join(".in-use.lock"));
        let _ = fs::remove_dir(&dir);
        Ok(true)
    }

    fn model_lock(&self, weights: &Weights) -> Result<File> {
        let dir = self.model_dir(weights);
        fs::create_dir_all(&dir)?;
        Ok(OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(".in-use.lock"))?)
    }
}

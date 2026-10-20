use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::forge::Snapshot;

const VERSION: u64 = 1;

/// The caller-owned file of baselines, locked through `<path>.lock` while open.
pub struct SnapshotFile {
    path: PathBuf,
    targets: BTreeMap<String, Entry>,
    _lock: File,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub forge_id: String,
    pub url: String,
    pub snapshot: Snapshot,
}

#[derive(Serialize, Deserialize)]
struct Contents {
    version: u64,
    targets: BTreeMap<String, Entry>,
}

#[derive(Deserialize)]
struct Versioned {
    version: u64,
}

#[derive(Debug)]
pub enum OpenError {
    /// The file can't be read as a version 1 snapshot file.
    Invalid(String),
    Io(io::Error),
    LockHeld(PathBuf),
}

impl SnapshotFile {
    pub fn open(path: &Path) -> Result<Self, OpenError> {
        let lock_path = with_suffix(path, ".lock");
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(OpenError::Io)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(OpenError::LockHeld(lock_path)),
            Err(TryLockError::Error(error)) => return Err(OpenError::Io(error)),
        }
        let targets = match fs::read_to_string(path) {
            Ok(text) => parse(&text).map_err(|reason| {
                OpenError::Invalid(format!(
                    "{} is not a babysit snapshot file: {reason}",
                    path.display()
                ))
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(OpenError::Io(error)),
        };
        Ok(Self {
            path: path.to_owned(),
            targets,
            _lock: lock,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entry(&self, url: &str) -> Option<&Entry> {
        self.targets.get(url)
    }

    /// Records `entry` under its URL and rewrites the file through a temp file and a rename.
    pub fn save(&mut self, entry: Entry) -> io::Result<()> {
        self.targets.insert(entry.url.clone(), entry);
        let contents = Contents {
            version: VERSION,
            targets: self.targets.clone(),
        };
        let temp_path = with_suffix(&self.path, ".tmp");
        fs::write(&temp_path, serde_json::to_string_pretty(&contents)? + "\n")?;
        fs::rename(&temp_path, &self.path)
    }
}

fn parse(text: &str) -> Result<BTreeMap<String, Entry>, String> {
    let Versioned { version } = serde_json::from_str(text).map_err(|error| error.to_string())?;
    if version != VERSION {
        return Err(format!("version {version} is not supported"));
    }
    let contents: Contents = serde_json::from_str(text).map_err(|error| error.to_string())?;
    Ok(contents.targets)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut path = OsString::from(path);
    path.push(suffix);
    path.into()
}

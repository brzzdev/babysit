use std::collections::BTreeMap;
use std::fs::{self, File, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::forge::Snapshot;

const VERSION: u64 = 1;

/// The caller-owned file of baselines, locked through `<path>.lock` while open.
pub struct SnapshotFile {
    path: PathBuf,
    contents: Contents,
    _lock: Lock,
}

/// An exclusive lock, released on drop: closing the file alone leaves it held while a child forked
/// mid-spawn still shares the descriptor.
struct Lock(File);

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

#[derive(Debug)]
pub enum OpenError {
    /// The file isn't a snapshot file of this version.
    Invalid(String),
    Io(io::Error),
    LockHeld,
}

impl SnapshotFile {
    pub fn open(path: &Path) -> Result<Self, OpenError> {
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.with_added_extension("lock"))
            .map_err(OpenError::Io)?;
        let lock = match lock.try_lock() {
            Ok(()) => Lock(lock),
            Err(TryLockError::WouldBlock) => return Err(OpenError::LockHeld),
            Err(TryLockError::Error(error)) => return Err(OpenError::Io(error)),
        };
        let contents = match fs::read(path) {
            Ok(bytes) => parse(&bytes).map_err(|reason| {
                OpenError::Invalid(format!(
                    "{} is not a babysit snapshot file: {reason}",
                    path.display()
                ))
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Contents {
                version: VERSION,
                targets: BTreeMap::new(),
            },
            Err(error) => return Err(OpenError::Io(error)),
        };
        Ok(Self {
            path: path.to_owned(),
            contents,
            _lock: lock,
        })
    }

    pub fn entry(&self, url: &str) -> Option<&Entry> {
        self.contents.targets.get(url)
    }

    /// Moves the first entry that `matches` under `url`, in memory until the next save.
    pub fn rename(&mut self, url: &str, matches: impl Fn(&Entry) -> bool) -> Option<&Entry> {
        let targets = &mut self.contents.targets;
        let from = targets.values().find(|entry| matches(entry))?.url.clone();
        let mut entry = targets.remove(&from)?;
        url.clone_into(&mut entry.url);
        Some(targets.entry(url.to_owned()).insert_entry(entry).into_mut())
    }

    /// Records each entry under its URL and replaces the file atomically, through a temp file.
    pub fn save(&mut self, entries: impl IntoIterator<Item = Entry>) -> io::Result<()> {
        for entry in entries {
            self.contents.targets.insert(entry.url.clone(), entry);
        }
        let temp_path = self.path.with_added_extension("tmp");
        let mut temp = File::create(&temp_path)?;
        serde_json::to_writer_pretty(&mut temp, &self.contents)?;
        writeln!(temp)?;
        temp.sync_all()?;
        fs::rename(&temp_path, &self.path)
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn parse(bytes: &[u8]) -> Result<Contents, String> {
    let contents: Contents = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    if contents.version != VERSION {
        return Err(format!("version {} is not supported", contents.version));
    }
    Ok(contents)
}

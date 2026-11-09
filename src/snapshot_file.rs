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

/// What every snapshot file version shares, which tells one from a foreign file.
#[derive(Deserialize)]
#[expect(dead_code, reason = "the fields only gate deserialisation")]
struct Header {
    version: u64,
    targets: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug)]
pub enum Error {
    /// The file isn't a snapshot file, or for `open`, not one of this version.
    Invalid(String),
    Io(io::Error),
    LockHeld,
}

impl SnapshotFile {
    pub fn open(path: &Path) -> Result<Self, Error> {
        let lock = Lock::take(&lock_path(path))?;
        let contents = match read(path)? {
            Some(bytes) => parse(&bytes).map_err(|reason| invalid(path, &reason))?,
            None => Contents {
                version: VERSION,
                targets: BTreeMap::new(),
            },
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
        let mut bytes = serde_json::to_vec_pretty(&self.contents)?;
        bytes.push(b'\n');
        replace(&self.path, &bytes)
    }
}

/// Replaces the file at `path` with `bytes` through `<path>.tmp`, so a reader sees all or nothing.
pub fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp_path = temp_path(path);
    let mut temp = File::create(&temp_path)?;
    temp.write_all(bytes)?;
    temp.sync_all()?;
    fs::rename(&temp_path, path)
}

/// Deletes the snapshot file of any version and its temp file, keeping `<path>.lock`: deleting the
/// lock while another process opens it would let two runs each lock a different file.
pub fn forget(path: &Path) -> Result<(), Error> {
    let lock_path = lock_path(path);
    if !path.try_exists().map_err(Error::Io)? && !lock_path.try_exists().map_err(Error::Io)? {
        return Ok(());
    }
    let _lock = Lock::take(&lock_path)?;
    if let Some(bytes) = read(path)? {
        serde_json::from_slice::<Header>(&bytes)
            .map_err(|error| invalid(path, &error.to_string()))?;
    }
    remove_if_present(path)?;
    remove_if_present(&temp_path(path))
}

impl Lock {
    fn take(lock_path: &Path) -> Result<Self, Error> {
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path)
            .map_err(Error::Io)?;
        match lock.try_lock() {
            Ok(()) => Ok(Self(lock)),
            Err(TryLockError::WouldBlock) => Err(Error::LockHeld),
            Err(TryLockError::Error(error)) => Err(Error::Io(error)),
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn lock_path(path: &Path) -> PathBuf {
    path.with_added_extension("lock")
}

fn temp_path(path: &Path) -> PathBuf {
    path.with_added_extension("tmp")
}

/// The file's bytes, or `None` when it doesn't exist.
fn read(path: &Path) -> Result<Option<Vec<u8>>, Error> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::Io(error)),
    }
}

fn invalid(path: &Path, reason: &str) -> Error {
    Error::Invalid(format!(
        "{} is not a babysit snapshot file: {reason}",
        path.display()
    ))
}

fn remove_if_present(path: &Path) -> Result<(), Error> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(Error::Io(error)),
        _ => Ok(()),
    }
}

fn parse(bytes: &[u8]) -> Result<Contents, String> {
    let contents: Contents = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    if contents.version != VERSION {
        return Err(format!("version {} is not supported", contents.version));
    }
    Ok(contents)
}

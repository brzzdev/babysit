use std::io::{self, Write};
use std::path::Path;

use crate::snapshot_file;

/// The file `--out` names, taking the envelope in place of stdout. What is written appears in
/// the file only on flush, whole.
pub struct OutFile<'a> {
    path: &'a Path,
    written: Vec<u8>,
}

impl<'a> OutFile<'a> {
    pub fn new(path: &'a Path) -> Self {
        Self {
            path,
            written: Vec::new(),
        }
    }
}

impl Write for OutFile<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.written.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    /// Replaces the file whole, or on failure removes it, so an earlier run's never stands in.
    fn flush(&mut self) -> io::Result<()> {
        snapshot_file::replace(self.path, &self.written).inspect_err(|_| {
            let _ = snapshot_file::remove_if_present(self.path);
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process;

    use super::*;

    #[test]
    fn a_write_that_fails_removes_the_earlier_runs_file() {
        let dir = std::env::temp_dir().join(format!("babysit-{}-out-file", process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("out.json");
        fs::create_dir_all(dir.join("out.json.tmp/blocking")).unwrap();
        fs::write(&path, "earlier").unwrap();

        let mut out = OutFile::new(&path);
        out.write_all(b"{}").unwrap();
        let flushed = out.flush();
        let left = path.exists();
        fs::remove_dir_all(&dir).unwrap();

        assert!(flushed.is_err());
        assert!(!left);
    }
}

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

    fn flush(&mut self) -> io::Result<()> {
        snapshot_file::replace(self.path, &self.written)
    }
}

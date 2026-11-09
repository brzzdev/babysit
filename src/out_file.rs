use std::fs;
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
    /// Takes `path`, deleting the envelope an earlier run left there, so a run that ends without
    /// writing one leaves no file to mistake for its own.
    pub fn create(path: &'a Path) -> io::Result<Self> {
        match fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(Self {
                path,
                written: Vec::new(),
            }),
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

#[cfg(test)]
mod tests {
    use std::process;

    use super::*;

    #[test]
    fn an_earlier_envelope_is_gone_once_the_file_is_taken_and_the_new_one_lands_on_flush() {
        let path = std::env::temp_dir().join(format!("babysit-{}-out-file.json", process::id()));
        fs::write(&path, "earlier").unwrap();

        let mut out = OutFile::create(&path).unwrap();
        let taken = path.exists();
        out.write_all(b"{}").unwrap();
        let unflushed = path.exists();
        out.flush().unwrap();
        let flushed = fs::read_to_string(&path).unwrap();
        fs::remove_file(&path).unwrap();

        assert!(!taken);
        assert!(!unflushed);
        assert_eq!(flushed, "{}");
    }
}

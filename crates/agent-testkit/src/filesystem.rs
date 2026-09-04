use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use zork_agent::session::ports::{FilePage, FileSystem};

#[derive(Clone, Default)]
pub struct MemoryFileSystem {
    files: Arc<Mutex<HashMap<PathBuf, Vec<u8>>>>,
}

impl MemoryFileSystem {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write_text(&self, path: impl AsRef<Path>, contents: &str) {
        self.write(path.as_ref(), contents.as_bytes())
            .expect("memory file write succeeds");
    }

    pub fn read_text(&self, path: impl AsRef<Path>) -> Option<String> {
        self.files
            .lock()
            .expect("memory filesystem lock poisoned")
            .get(path.as_ref())
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    }

    pub fn exists(&self, path: impl AsRef<Path>) -> bool {
        self.files
            .lock()
            .expect("memory filesystem lock poisoned")
            .contains_key(path.as_ref())
    }
}

impl FileSystem for MemoryFileSystem {
    fn create_dir_all(&self, _: &Path) -> std::io::Result<()> {
        Ok(())
    }

    fn create(&self, path: &Path) -> std::io::Result<Box<dyn Write + Send>> {
        self.files
            .lock()
            .expect("memory filesystem lock poisoned")
            .insert(path.to_owned(), Vec::new());
        Ok(Box::new(MemoryWriter {
            files: self.files.clone(),
            path: path.to_owned(),
        }))
    }

    fn read_page(&self, path: &Path, offset: u64, limit: usize) -> std::io::Result<FilePage> {
        let files = self.files.lock().expect("memory filesystem lock poisoned");
        let bytes = files.get(path).ok_or_else(not_found)?;
        let total_size = bytes.len() as u64;
        let start = offset.min(total_size) as usize;
        let end = start.saturating_add(limit).min(bytes.len());
        Ok(FilePage {
            bytes: bytes[start..end].to_vec(),
            offset,
            total_size,
            next_offset: (end < bytes.len()).then_some(end as u64),
        })
    }

    fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
        let files = self.files.lock().expect("memory filesystem lock poisoned");
        let bytes = files.get(path).ok_or_else(not_found)?;
        String::from_utf8(bytes.clone())
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    fn write(&self, path: &Path, contents: &[u8]) -> std::io::Result<()> {
        self.files
            .lock()
            .expect("memory filesystem lock poisoned")
            .insert(path.to_owned(), contents.to_vec());
        Ok(())
    }

    fn tail(&self, path: &Path, max_bytes: usize) -> std::io::Result<Vec<u8>> {
        let files = self.files.lock().expect("memory filesystem lock poisoned");
        let bytes = files.get(path).ok_or_else(not_found)?;
        let start = bytes.len().saturating_sub(max_bytes);
        let mut tail = bytes[start..].to_vec();
        while !tail.is_empty() && std::str::from_utf8(&tail).is_err() {
            tail.remove(0);
        }
        Ok(tail)
    }
}

struct MemoryWriter {
    files: Arc<Mutex<HashMap<PathBuf, Vec<u8>>>>,
    path: PathBuf,
}

impl Write for MemoryWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.files
            .lock()
            .expect("memory filesystem lock poisoned")
            .entry(self.path.clone())
            .or_default()
            .extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn not_found() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotFound, "virtual file does not exist")
}

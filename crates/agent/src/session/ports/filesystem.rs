use std::io::{Read, Seek, Write};
use std::path::Path;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilePage {
    pub bytes: Vec<u8>,
    pub offset: u64,
    pub total_size: u64,
    pub next_offset: Option<u64>,
}

pub trait FileSystem: Send + Sync {
    fn create_dir_all(&self, path: &Path) -> std::io::Result<()>;

    fn create(&self, path: &Path) -> std::io::Result<Box<dyn Write + Send>>;

    fn read_page(&self, path: &Path, offset: u64, limit: usize) -> std::io::Result<FilePage>;

    fn read_to_string(&self, path: &Path) -> std::io::Result<String>;

    fn write(&self, path: &Path, contents: &[u8]) -> std::io::Result<()>;

    fn tail(&self, path: &Path, max_bytes: usize) -> std::io::Result<Vec<u8>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemFileSystem;

impl FileSystem for SystemFileSystem {
    fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn create(&self, path: &Path) -> std::io::Result<Box<dyn Write + Send>> {
        std::fs::File::create(path).map(|file| Box::new(file) as Box<dyn Write + Send>)
    }

    fn read_page(&self, path: &Path, offset: u64, limit: usize) -> std::io::Result<FilePage> {
        let mut file = std::fs::File::open(path)?;
        let total_size = file.metadata()?.len();
        let start = offset.min(total_size);
        file.seek(std::io::SeekFrom::Start(start))?;
        let length = total_size.saturating_sub(start).min(limit as u64) as usize;
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes)?;
        let end = start + length as u64;
        Ok(FilePage {
            bytes,
            offset,
            total_size,
            next_offset: (end < total_size).then_some(end),
        })
    }

    fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn write(&self, path: &Path, contents: &[u8]) -> std::io::Result<()> {
        std::fs::write(path, contents)
    }

    fn tail(&self, path: &Path, max_bytes: usize) -> std::io::Result<Vec<u8>> {
        let mut file = std::fs::File::open(path)?;
        let size = file.seek(std::io::SeekFrom::End(0))?;
        let start = size.saturating_sub(max_bytes as u64);
        file.seek(std::io::SeekFrom::Start(start))?;
        let mut bytes = Vec::with_capacity((size - start) as usize);
        file.read_to_end(&mut bytes)?;
        while !bytes.is_empty() && std::str::from_utf8(&bytes).is_err() {
            bytes.remove(0);
        }
        Ok(bytes)
    }
}

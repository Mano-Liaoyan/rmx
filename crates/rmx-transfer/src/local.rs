//! [`RemoteFs`] over the local file system.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::task::spawn_blocking;

use crate::error::{Error, Result};
use crate::fs::{
    BatchBuffer, DirStream, Entry, EntryKind, ListOptions, RangeRead, RangeWrite, RemoteFs,
    WriteStart, base_name,
};

/// Default size of the chunks produced by [`LocalFs::read_range`](RemoteFs::read_range).
pub const DEFAULT_LOCAL_CHUNK: usize = 256 * 1024;

/// The local file system. Blocking calls run on Tokio's blocking pool.
#[derive(Debug, Clone)]
pub struct LocalFs {
    chunk_size: usize,
}

impl Default for LocalFs {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalFs {
    /// Creates a backend that reads in [`DEFAULT_LOCAL_CHUNK`] chunks.
    pub fn new() -> Self {
        Self { chunk_size: DEFAULT_LOCAL_CHUNK }
    }

    /// Creates a backend with a specific read chunk size, at least one byte.
    pub fn with_chunk_size(chunk_size: usize) -> Self {
        Self { chunk_size: chunk_size.max(1) }
    }
}

fn entry_from(name: String, meta: &fs::Metadata) -> Entry {
    let file_type = meta.file_type();
    let kind = if file_type.is_file() {
        EntryKind::File
    } else if file_type.is_dir() {
        EntryKind::Dir
    } else if file_type.is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::Other
    };
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::MetadataExt;
        Some(meta.mode())
    };
    #[cfg(not(unix))]
    let mode = None;
    Entry { name, kind, size: Some(meta.len()), modified: meta.modified().ok(), mode }
}

async fn blocking<T, F>(f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    spawn_blocking(f).await?
}

#[async_trait]
impl RemoteFs for LocalFs {
    async fn list(&self, path: &str, options: ListOptions) -> Result<DirStream> {
        let path = path.to_owned();
        let (tx, stream) = DirStream::channel();
        // Open first so that an unreadable directory fails here rather than in the stream.
        let dir = {
            let path = path.clone();
            blocking(move || fs::read_dir(&path).map_err(|e| Error::from_io(&path, e))).await?
        };
        spawn_blocking(move || {
            let mut buffer = BatchBuffer::new(options);
            for item in dir {
                let entry = item
                    .and_then(|item| {
                        let meta = item.metadata()?;
                        Ok(entry_from(item.file_name().to_string_lossy().into_owned(), &meta))
                    })
                    .map_err(|e| Error::from_io(&path, e));
                match entry {
                    Ok(entry) => {
                        if let Some(batch) = buffer.push(entry)
                            && tx.blocking_send(Ok(batch)).is_err()
                        {
                            return;
                        }
                    },
                    Err(error) => {
                        let _ = tx.blocking_send(Err(error));
                        return;
                    },
                }
            }
            if let Some(batch) = buffer.drain() {
                let _ = tx.blocking_send(Ok(batch));
            }
        });
        Ok(stream)
    }

    async fn stat(&self, path: &str) -> Result<Entry> {
        let path = path.to_owned();
        blocking(move || {
            let meta = fs::metadata(&path).map_err(|e| Error::from_io(&path, e))?;
            Ok(entry_from(base_name(&path), &meta))
        })
        .await
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        let path = path.to_owned();
        blocking(move || fs::create_dir(&path).map_err(|e| Error::from_io(&path, e))).await
    }

    async fn remove_file(&self, path: &str) -> Result<()> {
        let path = path.to_owned();
        blocking(move || fs::remove_file(&path).map_err(|e| Error::from_io(&path, e))).await
    }

    async fn remove_dir(&self, path: &str) -> Result<()> {
        let path = path.to_owned();
        blocking(move || fs::remove_dir(&path).map_err(|e| Error::from_io(&path, e))).await
    }

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (from, to) = (from.to_owned(), to.to_owned());
        blocking(move || fs::rename(&from, &to).map_err(|e| Error::from_io(&from, e))).await
    }

    async fn read_range(
        &self,
        path: &str,
        offset: u64,
        len: Option<u64>,
    ) -> Result<Box<dyn RangeRead>> {
        let chunk = self.chunk_size;
        let path = path.to_owned();
        let name = path.clone();
        let file = blocking(move || {
            let mut file = File::open(&path).map_err(|e| Error::from_io(&path, e))?;
            file.seek(SeekFrom::Start(offset)).map_err(|e| Error::from_io(&path, e))?;
            Ok(file)
        })
        .await?;
        Ok(Box::new(LocalReader { file: Some(file), remaining: len, chunk, path: name }))
    }

    async fn write_range(&self, path: &str, start: WriteStart) -> Result<Box<dyn RangeWrite>> {
        let path = path.to_owned();
        let name = path.clone();
        let file = blocking(move || {
            let mut options = fs::OpenOptions::new();
            options.write(true).create(true);
            if matches!(start, WriteStart::Truncate) {
                options.truncate(true);
            }
            let mut file = options.open(&path).map_err(|e| Error::from_io(&path, e))?;
            if let WriteStart::At(offset) = start {
                file.seek(SeekFrom::Start(offset)).map_err(|e| Error::from_io(&path, e))?;
            }
            Ok(file)
        })
        .await?;
        Ok(Box::new(LocalWriter { file: Some(file), path: name }))
    }
}

struct LocalReader {
    file: Option<File>,
    remaining: Option<u64>,
    chunk: usize,
    path: String,
}

#[async_trait]
impl RangeRead for LocalReader {
    async fn next_chunk(&mut self) -> Result<Option<Bytes>> {
        let want = match self.remaining {
            Some(0) => return Ok(None),
            Some(remaining) => remaining.min(self.chunk as u64) as usize,
            None => self.chunk,
        };
        let Some(mut file) = self.file.take() else { return Ok(None) };
        let path = self.path.clone();
        let (file, data) = blocking(move || {
            let mut buffer = vec![0u8; want];
            let read = file.read(&mut buffer).map_err(|e| Error::from_io(&path, e))?;
            buffer.truncate(read);
            Ok((file, buffer))
        })
        .await?;
        if data.is_empty() {
            return Ok(None);
        }
        self.file = Some(file);
        if let Some(remaining) = &mut self.remaining {
            *remaining -= data.len() as u64;
        }
        Ok(Some(Bytes::from(data)))
    }
}

struct LocalWriter {
    file: Option<File>,
    path: String,
}

#[async_trait]
impl RangeWrite for LocalWriter {
    async fn write(&mut self, chunk: Bytes) -> Result<()> {
        let mut file = self.file.take().ok_or_else(|| Error::Io("writer is closed".into()))?;
        let path = self.path.clone();
        let file = blocking(move || {
            file.write_all(&chunk).map_err(|e| Error::from_io(&path, e))?;
            Ok(file)
        })
        .await?;
        self.file = Some(file);
        Ok(())
    }

    async fn finish(mut self: Box<Self>) -> Result<()> {
        let Some(mut file) = self.file.take() else { return Ok(()) };
        let path = std::mem::take(&mut self.path);
        blocking(move || file.flush().map_err(|e| Error::from_io(&path, e))).await
    }
}

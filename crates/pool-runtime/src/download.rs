//! Locked, resumable, verified downloads. A file appears at its final path
//! only after its exact size and SHA-256 match; partial data lives in `.part`.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use futures_util::StreamExt;
use reqwest::{StatusCode, header};
use sha2::{Digest, Sha256};

use crate::{Error, Progress, Result, Stage};

const ATTEMPTS: u32 = 6;

/// Ensures `dest` holds the expected content, downloading it if necessary.
pub async fn fetch_verified(
    http: &reqwest::Client,
    url: &str,
    dest: &Path,
    size: u64,
    sha256: &str,
    progress: Progress<'_>,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let _lock = lock(dest).await?;

    if dest.exists() {
        match verify_file(dest, size, sha256, progress).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                tracing::warn!("{} failed verification ({error}); downloading again", dest.display());
                fs::remove_file(dest)?;
            }
        }
    }

    let part = with_suffix(dest, ".part");
    let mut last_error = None;
    for attempt in 1..=ATTEMPTS {
        match download_part(http, url, &part, size, progress).await {
            Ok(()) => match verify_file(&part, size, sha256, progress).await {
                Ok(()) => {
                    fs::rename(&part, dest)?;
                    return Ok(());
                }
                Err(error) => {
                    // corrupt content cannot be resumed; start over
                    let _ = fs::remove_file(&part);
                    last_error = Some(error);
                }
            },
            Err(error) => last_error = Some(error),
        }
        if attempt < ATTEMPTS {
            let wait = Duration::from_secs(1 << (attempt - 1).min(4));
            tracing::warn!(
                "download of {url} failed (attempt {attempt}/{ATTEMPTS}): {}; retrying in {}s",
                last_error.as_ref().unwrap(),
                wait.as_secs()
            );
            tokio::time::sleep(wait).await;
        }
    }
    Err(Error::msg(format!("download of {url} failed after {ATTEMPTS} attempts: {}", last_error.unwrap())))
}

/// Appends to `part` until it holds `size` bytes, resuming with HTTP Range.
async fn download_part(
    http: &reqwest::Client,
    url: &str,
    part: &Path,
    size: u64,
    progress: Progress<'_>,
) -> Result<()> {
    let mut existing = fs::metadata(part).map(|meta| meta.len()).unwrap_or(0);
    if existing > size {
        existing = 0;
    }
    if existing == size {
        return Ok(());
    }

    let mut request = http.get(url);
    if existing > 0 {
        tracing::info!("resuming {} at {existing} of {size} bytes", part.display());
        request = request.header(header::RANGE, format!("bytes={existing}-"));
    }
    let response = request.send().await?;
    let status = response.status();

    let mut file = OpenOptions::new().create(true).write(true).truncate(false).open(part)?;
    let mut written = match status {
        StatusCode::PARTIAL_CONTENT => {
            let range = response.headers().get(header::CONTENT_RANGE).and_then(|value| value.to_str().ok());
            if !range.is_some_and(|range| range.starts_with(&format!("bytes {existing}-"))) {
                return Err(Error::msg(format!("server sent an unexpected Content-Range {range:?}")));
            }
            file.seek(SeekFrom::Start(existing))?;
            existing
        }
        StatusCode::OK => {
            // the server ignored Range: restart from zero
            if existing > 0 {
                tracing::warn!("server ignored the resume range; restarting from zero");
            }
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            if let Some(length) = response.content_length()
                && length != size
            {
                return Err(Error::msg(format!("server reports {length} bytes, expected {size}")));
            }
            0
        }
        StatusCode::RANGE_NOT_SATISFIABLE => {
            file.set_len(0)?;
            return Err(Error::msg("server rejected the resume range; restarting"));
        }
        status => return Err(Error::msg(format!("HTTP {status} from {url}"))),
    };

    progress(Stage::Downloading, written, size);
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        if written + chunk.len() as u64 > size {
            file.set_len(0)?;
            return Err(Error::msg(format!("server sent more than the expected {size} bytes")));
        }
        file.write_all(&chunk)?;
        written += chunk.len() as u64;
        progress(Stage::Downloading, written, size);
    }
    file.sync_all()?;
    if written != size {
        return Err(Error::msg(format!("connection closed at {written} of {size} bytes")));
    }
    Ok(())
}

/// Checks exact size, then the full SHA-256.
pub async fn verify_file(path: &Path, size: u64, sha256: &str, progress: Progress<'_>) -> Result<()> {
    let actual = fs::metadata(path)?.len();
    if actual != size {
        return Err(Error::msg(format!("{} has {actual} bytes, expected {size}", path.display())));
    }
    let digest = hash_file(path, size, progress).await?;
    if digest != sha256 {
        return Err(Error::msg(format!("{} has SHA-256 {digest}, expected {sha256}", path.display())));
    }
    Ok(())
}

async fn hash_file(path: &Path, size: u64, progress: Progress<'_>) -> Result<String> {
    // hash on a blocking thread, reporting progress back to this task
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let path = path.to_path_buf();
    let worker = tokio::task::spawn_blocking(move || -> Result<String> {
        let mut file = File::open(&path)?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 4 << 20];
        let mut done = 0u64;
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            done += n as u64;
            let _ = tx.send(done);
        }
        Ok(pool_protocol::catalog::hex(&hasher.finalize()))
    });
    while let Some(done) = rx.recv().await {
        progress(Stage::Verifying, done, size);
    }
    worker.await.map_err(|error| Error::msg(format!("hashing failed: {error}")))?
}

/// Verifies a local file and places it at `dest` (hard link, else copy).
pub async fn import_verified(
    source: &Path,
    dest: &Path,
    size: u64,
    sha256: &str,
    progress: Progress<'_>,
) -> Result<()> {
    verify_file(source, size, sha256, progress).await?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let _lock = lock(dest).await?;
    if dest.exists() {
        fs::remove_file(dest)?;
    }
    if fs::hard_link(source, dest).is_err() {
        let part = with_suffix(dest, ".part");
        fs::copy(source, &part)?;
        verify_file(&part, size, sha256, progress).await?;
        fs::rename(&part, dest)?;
    }
    Ok(())
}

/// Exclusive lock beside `dest`, waiting for any other process downloading it.
async fn lock(dest: &Path) -> Result<File> {
    let path = with_suffix(dest, ".lock");
    let file = OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
    if file.try_lock().is_ok() {
        return Ok(file);
    }
    tracing::info!("waiting for another process to finish {}", dest.display());
    let file = tokio::task::spawn_blocking(move || file.lock().map(|()| file))
        .await
        .map_err(|error| Error::msg(format!("lock wait failed: {error}")))??;
    Ok(file)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

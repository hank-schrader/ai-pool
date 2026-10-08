//! Installs an approved runtime target: verified archives extracted safely into
//! one directory, then the binary must report the pinned llama.cpp commit.

use std::{
    fs::{self, File},
    io,
    path::{Component, Path, PathBuf},
    process::Stdio,
};

use crate::{
    Cache, Error, Progress, Result, Stage,
    download::fetch_verified,
    manifest::{RuntimeSpec, TargetSpec},
};

const MARKER: &str = ".ai-pool-installed";

#[derive(Clone, Debug)]
pub struct InstalledRuntime {
    pub dir: PathBuf,
    pub server: PathBuf,
    /// First line of `llama-server --version` that names the commit.
    pub version: String,
}

/// Installs the target if needed and checks the binary. `override_server`
/// replaces the managed binary but must pass the same version check.
pub async fn ensure_runtime(
    http: &reqwest::Client,
    cache: &Cache,
    runtime: &RuntimeSpec,
    target: &TargetSpec,
    override_server: Option<&Path>,
    progress: Progress<'_>,
) -> Result<InstalledRuntime> {
    if let Some(server) = override_server {
        let version = check_version(server, &runtime.llama_cpp_commit).await?;
        let dir = server.parent().map(Path::to_path_buf).unwrap_or_default();
        return Ok(InstalledRuntime { dir, server: server.to_path_buf(), version });
    }

    let dir = cache.runtime_dir(&runtime.id, &target.target);
    let expected_marker = marker_text(target);
    let installed = fs::read_to_string(dir.join(MARKER)).is_ok_and(|marker| marker == expected_marker);
    if !installed {
        install(http, cache, target, &dir, &expected_marker, progress).await?;
    }
    let server = dir.join(&target.server_binary);
    let version = check_version(&server, &runtime.llama_cpp_commit).await?;
    Ok(InstalledRuntime { dir, server, version })
}

/// Whether the target is installed (marker present), without checking it.
pub fn is_installed(cache: &Cache, runtime: &RuntimeSpec, target: &TargetSpec) -> bool {
    let dir = cache.runtime_dir(&runtime.id, &target.target);
    fs::read_to_string(dir.join(MARKER)).is_ok_and(|marker| marker == marker_text(target))
}

fn marker_text(target: &TargetSpec) -> String {
    target.archives.iter().map(|archive| format!("{} {}\n", archive.sha256, archive.name)).collect()
}

async fn install(
    http: &reqwest::Client,
    cache: &Cache,
    target: &TargetSpec,
    dir: &Path,
    marker: &str,
    progress: Progress<'_>,
) -> Result<()> {
    let mut archives = Vec::new();
    for archive in &target.archives {
        let path = cache.download_path(&archive.sha256, &archive.name);
        tracing::info!("fetching runtime archive {}", archive.name);
        fetch_verified(http, &archive.url, &path, archive.size_bytes, &archive.sha256, progress).await?;
        archives.push(path);
    }

    let parent = dir.parent().ok_or_else(|| Error::msg("runtime directory has no parent"))?;
    fs::create_dir_all(parent)?;
    let staging = parent.join(format!(".staging-{}", uuid::Uuid::new_v4()));
    let total = archives.len() as u64;
    let unpack_all = || -> Result<()> {
        fs::create_dir_all(&staging)?;
        for (index, archive) in archives.iter().enumerate() {
            progress(Stage::Extracting, index as u64, total);
            let unpack = staging.join(format!(".unpack-{index}"));
            fs::create_dir_all(&unpack)?;
            extract(archive, &unpack)?;
            merge_stripped(&unpack, &staging)?;
            fs::remove_dir_all(&unpack)?;
        }
        progress(Stage::Extracting, total, total);
        Ok(())
    };
    // extraction is synchronous file work; keep it off the async workers
    let result = tokio::task::block_in_place(unpack_all);
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    fs::rename(&staging, dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let server = dir.join(&target.server_binary);
        if server.exists() {
            fs::set_permissions(&server, fs::Permissions::from_mode(0o755))?;
        }
    }
    fs::write(dir.join(MARKER), marker)?;
    Ok(())
}

fn extract(archive: &Path, dest: &Path) -> Result<()> {
    let name = archive.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        extract_tar_gz(archive, dest)
    } else if name.ends_with(".zip") {
        extract_zip(archive, dest)
    } else {
        Err(Error::msg(format!("unsupported archive format: {name}")))
    }
}

/// Relative, normalized path inside the destination, or None if it escapes.
fn safe_relative(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if out.as_os_str().is_empty() { None } else { Some(out) }
}

/// A symlink target is allowed only if it resolves inside the destination.
fn link_stays_inside(entry: &Path, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let mut depth: i64 = entry.components().count() as i64 - 1;
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<()> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(File::open(archive)?));
    for entry in tar.entries()? {
        let mut entry = entry?;
        let raw = entry.path()?.into_owned();
        let Some(relative) = safe_relative(&raw) else {
            return Err(Error::msg(format!("archive entry {} escapes the install directory", raw.display())));
        };
        let out = dest.join(&relative);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            fs::create_dir_all(&out)?;
        } else if kind.is_file() {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = File::create(&out)?;
            io::copy(&mut entry, &mut file)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = entry.header().mode().unwrap_or(0o644) & 0o755;
                fs::set_permissions(&out, fs::Permissions::from_mode(mode))?;
            }
        } else if kind.is_symlink() {
            let target = entry
                .link_name()?
                .ok_or_else(|| Error::msg(format!("symlink {} has no target", raw.display())))?
                .into_owned();
            if !link_stays_inside(&relative, &target) {
                return Err(Error::msg(format!("symlink {} points outside the install directory", raw.display())));
            }
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)?;
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &out)?;
            #[cfg(not(unix))]
            return Err(Error::msg(format!("symlink {} is not supported on this platform", raw.display())));
        } else if matches!(kind, tar::EntryType::XGlobalHeader | tar::EntryType::XHeader) {
            // pax metadata, already applied to the following entry
        } else {
            return Err(Error::msg(format!("unsupported archive entry type for {}", raw.display())));
        }
    }
    Ok(())
}

fn extract_zip(archive: &Path, dest: &Path) -> Result<()> {
    let mut zip = zip::ZipArchive::new(File::open(archive)?).map_err(|error| Error::msg(error.to_string()))?;
    for index in 0..zip.len() {
        let mut file = zip.by_index(index).map_err(|error| Error::msg(error.to_string()))?;
        let Some(relative) = file.enclosed_name().and_then(|name| safe_relative(&name)) else {
            return Err(Error::msg(format!("archive entry {} escapes the install directory", file.name())));
        };
        if file.is_symlink() {
            return Err(Error::msg(format!("symlink {} in a zip archive is not supported", file.name())));
        }
        let out = dest.join(&relative);
        if file.is_dir() {
            fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut target = File::create(&out)?;
        io::copy(&mut file, &mut target)?;
        #[cfg(unix)]
        if let Some(mode) = file.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&out, fs::Permissions::from_mode(mode & 0o755))?;
        }
    }
    Ok(())
}

/// Moves the unpacked tree into `dest`, stripping a single top-level directory.
/// Two archives may not provide the same file.
fn merge_stripped(unpack: &Path, dest: &Path) -> Result<()> {
    let entries: Vec<_> = fs::read_dir(unpack)?.collect::<io::Result<_>>()?;
    let root = match entries.as_slice() {
        [only] if only.file_type()?.is_dir() => only.path(),
        _ => unpack.to_path_buf(),
    };
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let target = dest.join(entry.file_name());
        if fs::symlink_metadata(&target).is_ok() {
            return Err(Error::msg(format!(
                "two runtime archives both contain {}",
                entry.file_name().to_string_lossy()
            )));
        }
        fs::rename(entry.path(), target)?;
    }
    Ok(())
}

/// Runs `--version` with a minimal environment: the bundle must be
/// self-contained, and the reported commit must be the pinned one.
pub async fn check_version(server: &Path, commit: &str) -> Result<String> {
    if !server.is_file() {
        return Err(Error::msg(format!("llama-server not found at {}", server.display())));
    }
    let mut command = tokio::process::Command::new(server);
    command.arg("--version").env_clear().stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let keep: &[&str] = if cfg!(windows) { &["SystemRoot", "windir", "PATH", "TEMP", "TMP"] } else { &["TMPDIR"] };
    for name in keep {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let output = tokio::time::timeout(std::time::Duration::from_secs(60), command.output())
        .await
        .map_err(|_| Error::msg(format!("{} --version timed out", server.display())))??;
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    let line = text.lines().find(|line| line.contains("commit ")).unwrap_or_default().trim().to_string();
    let reported = line
        .split("commit ")
        .nth(1)
        .map(|rest| rest.trim_end_matches(')').split_whitespace().next().unwrap_or_default())
        .unwrap_or_default();
    let clean = reported.len() >= 7 && reported.bytes().all(|b| b.is_ascii_hexdigit());
    if !output.status.success() || !clean || !commit.starts_with(reported) {
        let shown = if line.is_empty() { text.lines().last().unwrap_or_default().to_string() } else { line };
        return Err(Error::msg(format!(
            "{} is not the pinned llama.cpp {} (reported: {shown:?}, exit {})",
            server.display(),
            &commit[..9],
            output.status
        )));
    }
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escaping_paths_and_links() {
        assert!(safe_relative(Path::new("../evil")).is_none());
        assert!(safe_relative(Path::new("/etc/passwd")).is_none());
        assert_eq!(safe_relative(Path::new("./a/b")), Some(PathBuf::from("a/b")));
        assert!(link_stays_inside(Path::new("top/libllama.so"), Path::new("libllama.so.0")));
        assert!(!link_stays_inside(Path::new("top/lib.so"), Path::new("../../outside")));
        assert!(!link_stays_inside(Path::new("lib.so"), Path::new("/usr/lib/lib.so")));
    }
}

//! System prerequisites, checked without executing the runtime's entry point.

use std::{collections::BTreeSet, path::Path, time::Duration};

use crate::{Error, Result};

const DISK_RESERVE: u64 = 512 << 20;

#[derive(Debug)]
pub struct Check {
    pub name: &'static str,
    pub status: &'static str,
    pub message: String,
}

impl Check {
    pub fn ok(name: &'static str, message: impl Into<String>) -> Self {
        Self { name, status: "ok", message: message.into() }
    }

    pub fn fail(name: &'static str, message: impl Into<String>) -> Self {
        Self { name, status: "fail", message: message.into() }
    }

    pub fn line(&self) -> String {
        format!("{:<7}{}: {}", format!("[{}]", self.status), self.name, self.message)
    }
}

pub fn ensure(checks: &[Check]) -> Result<()> {
    let errors: Vec<_> = checks.iter().filter(|check| check.status == "fail").map(Check::line).collect();
    if errors.is_empty() { Ok(()) } else { Err(Error::msg(errors.join("\n"))) }
}

pub fn driver(actual: &str, minimum: &str) -> Check {
    fn version(text: &str) -> Option<Vec<u32>> {
        let mut parts: Vec<u32> = text.split('.').map(str::parse).collect::<std::result::Result<_, _>>().ok()?;
        parts.resize(parts.len().max(4), 0);
        Some(parts)
    }
    match (version(actual), version(minimum)) {
        (Some(actual_parts), Some(minimum_parts)) if actual_parts >= minimum_parts => {
            Check::ok("NVIDIA driver", format!("{actual} (required >= {minimum})"))
        }
        _ => Check::fail(
            "NVIDIA driver",
            format!(
                "found {actual:?}; this runtime requires >= {minimum}. Update the host NVIDIA driver; in containers, pass the GPU through with NVIDIA Container Toolkit."
            ),
        ),
    }
}

/// Cached complete files and resumable partial files need no duplicate storage.
pub fn download_bytes(path: &Path, size: u64) -> u64 {
    if std::fs::metadata(path).is_ok_and(|meta| meta.len() == size) {
        return 0;
    }
    let mut part = path.as_os_str().to_os_string();
    part.push(".part");
    let saved = std::fs::metadata(Path::new(&part)).map_or(0, |meta| meta.len());
    size.saturating_sub(if saved <= size { saved } else { 0 })
}

pub fn disk_bytes(downloads: u64, archives_to_extract: u64) -> u64 {
    let needed = downloads.saturating_add(archives_to_extract.saturating_mul(4));
    if needed == 0 { 0 } else { needed.saturating_add(DISK_RESERVE) }
}

pub fn free_bytes(path: &Path) -> Result<u64> {
    let mut existing = path;
    while !existing.exists() {
        existing = existing.parent().ok_or_else(|| Error::msg("cannot locate the cache filesystem"))?;
        if existing.as_os_str().is_empty() {
            existing = Path::new(".");
        }
    }
    let resolved = std::fs::canonicalize(existing)?;
    sysinfo::Disks::new_with_refreshed_list()
        .iter()
        .filter(|disk| resolved.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().components().count())
        .map(|disk| disk.available_space())
        .ok_or_else(|| Error::msg(format!("cannot read free disk space for {}", resolved.display())))
}

pub fn disk(path: &Path, needed: u64, free: u64) -> Check {
    let message = format!(
        "{}: {:.2} GB free, {:.2} GB needed (downloads + estimated extraction + reserve)",
        path.display(),
        free as f64 / 1e9,
        needed as f64 / 1e9
    );
    if free >= needed {
        Check::ok("disk", message)
    } else {
        Check::fail("disk", format!("{message}. Free disk space or select another --cache-dir."))
    }
}

/// Match the launcher's bundled-library path, including user-supplied paths.
pub fn library_path(command: &mut tokio::process::Command, server: &Path) -> Result<()> {
    if cfg!(target_os = "linux") {
        let dir = std::fs::canonicalize(server)?
            .parent()
            .ok_or_else(|| Error::msg("runtime has no directory"))?
            .to_path_buf();
        let mut paths = vec![dir];
        if let Some(existing) = std::env::var_os("LD_LIBRARY_PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        command.env("LD_LIBRARY_PATH", std::env::join_paths(paths).map_err(|error| Error::msg(error.to_string()))?);
    }
    Ok(())
}

fn install_hint(library: &str, release: &str) -> String {
    let get = |key: &str| {
        release.lines().find_map(|line| line.strip_prefix(&format!("{key}="))).unwrap_or("").trim_matches(['\'', '"'])
    };
    let id = get("ID");
    let family: Vec<_> = std::iter::once(id).chain(get("ID_LIKE").split_whitespace()).collect();
    if library == "libcuda.so.1" {
        return "install/update the host NVIDIA driver, or pass the GPU into the container with NVIDIA Container Toolkit".into();
    }
    let package = match library {
        "libgomp.so.1" => ["libgomp1", "libgomp", "gcc-libs"],
        "libssl.so.3" | "libcrypto.so.3" => [
            if id == "ubuntu"
                && get("VERSION_ID")
                    .split('.')
                    .next()
                    .and_then(|major| major.parse::<u32>().ok())
                    .is_some_and(|major| major >= 24)
                || id == "debian" && get("VERSION_ID").parse::<u32>().is_ok_and(|major| major >= 13)
            {
                "libssl3t64"
            } else {
                "libssl3"
            },
            "openssl-libs",
            "openssl",
        ],
        "libstdc++.so.6" => ["libstdc++6", "libstdc++", "gcc-libs"],
        _ => {
            return format!(
                "install the system package providing {library}; if it belongs to the runtime bundle, reinstall the runtime"
            );
        }
    };
    if family.iter().any(|id| matches!(*id, "ubuntu" | "debian")) {
        format!("sudo apt-get install {}", package[0])
    } else if family.iter().any(|id| matches!(*id, "fedora" | "rhel" | "centos")) {
        format!("sudo dnf install {}", package[1])
    } else if family.contains(&"arch") {
        format!("sudo pacman -S {}", package[2])
    } else {
        format!("install {library} using your distribution's package manager")
    }
}

/// Prerequisites of the pinned Linux ARM runtime, also checked before it is installed.
pub fn platform() -> Vec<Check> {
    #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]
    {
        let version = unsafe { std::ffi::CStr::from_ptr(libc::gnu_get_libc_version()) }.to_string_lossy();
        let mut checks = vec![glibc(&version)];
        let release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        for library in ["libgomp.so.1", "libssl.so.3", "libcrypto.so.3"] {
            let name = std::ffi::CString::new(library).expect("library name");
            let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
            if handle.is_null() {
                checks.push(Check::fail(
                    "system library",
                    format!("missing {library}; {}", install_hint(library, &release)),
                ));
            } else {
                unsafe { libc::dlclose(handle) };
                checks.push(Check::ok("system library", library));
            }
        }
        checks
    }
    #[cfg(not(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu")))]
    Vec::new()
}

#[cfg(any(test, all(target_os = "linux", target_arch = "aarch64", target_env = "gnu")))]
fn glibc(version: &str) -> Check {
    let parts: Option<Vec<u32>> = version.split('.').map(|part| part.parse().ok()).collect();
    if parts.is_some_and(|parts| parts.as_slice() >= [2, 38].as_slice()) {
        Check::ok("glibc", format!("{version} (required >= 2.38)"))
    } else {
        Check::fail(
            "glibc",
            format!(
                "{version:?}; the ARM runtime requires >= 2.38. Upgrade to Fedora 39+ or Ubuntu 24.04+ (Debian 13+)."
            ),
        )
    }
}

pub fn loader_output(text: &str, release: &str) -> Vec<Check> {
    let mut problems = BTreeSet::new();
    for line in
        text.lines().filter(|line| line.contains("not found") || line.contains("cannot open shared object file"))
    {
        if let Some(version) = line.split("version ").nth(1).and_then(|text| text.split_whitespace().next()) {
            let version = version.trim_matches(['`', '\'', '"']);
            let hint = if version.starts_with("GLIBC_")
                || version.starts_with("GLIBCXX_")
                || version.starts_with("CXXABI_")
            {
                "this OS is too old for the runtime: use Ubuntu 24.04+, Debian 13+, Fedora 39+, or a current Arch system (glibc >= 2.38 and a current libstdc++)".to_owned()
            } else {
                "update the system package providing this symbol version".into()
            };
            problems.insert(format!("missing symbol version {version}; {hint}"));
        } else if let Some((library, _)) = line.trim().split_once(" =>") {
            problems.insert(format!("missing {library}; {}", install_hint(library, release)));
        } else if let Some(library) =
            line.split("error while loading shared libraries: ").nth(1).and_then(|text| text.split(':').next())
        {
            problems.insert(format!("missing {library}; {}", install_hint(library, release)));
        }
    }
    problems.into_iter().map(|problem| Check::fail("Linux loader", problem)).collect()
}

pub async fn loader(server: &Path) -> Result<Vec<Check>> {
    if !cfg!(target_os = "linux") {
        return Ok(vec![Check {
            name: "Linux loader",
            status: "--",
            message: "not applicable on this platform".into(),
        }]);
    }
    let server = std::fs::canonicalize(server)?;
    let Some(interpreter) = interpreter(&server)? else {
        return Ok(vec![Check {
            name: "Linux loader",
            status: "--",
            message: "binary has no ELF interpreter (static binary or --llama-server wrapper)".into(),
        }]);
    };
    let dir = server.parent().ok_or_else(|| Error::msg("runtime has no directory"))?;
    let mut files = BTreeSet::from([server.clone()]);
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_file() && path.file_name().is_some_and(|name| name.to_string_lossy().contains(".so")) {
            files.insert(std::fs::canonicalize(path)?);
        }
    }
    let release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let mut checks = Vec::new();
    for file in files {
        let mut command = tokio::process::Command::new(&interpreter);
        command
            .arg("--list")
            .arg(&file)
            .env("LC_ALL", "C")
            .env_remove("LD_PRELOAD")
            .env_remove("LD_AUDIT")
            .kill_on_drop(true);
        library_path(&mut command, &server)?;
        let output = tokio::time::timeout(Duration::from_secs(10), command.output())
            .await
            .map_err(|_| Error::msg("Linux loader dependency check timed out"))??;
        let text = format!("{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        let found = loader_output(&text, &release);
        if found.is_empty() && !output.status.success() {
            checks.push(Check::fail(
                "Linux loader",
                format!("{}: {}", file.display(), text.trim().chars().take(500).collect::<String>()),
            ));
        }
        checks.extend(found);
    }
    checks.sort_by(|a, b| a.message.cmp(&b.message));
    checks.dedup_by(|a, b| a.message == b.message);
    if checks.is_empty() {
        checks.push(Check::ok(
            "Linux loader",
            "runtime and bundled libraries resolve; required symbol versions are available",
        ));
    }
    Ok(checks)
}

fn interpreter(path: &Path) -> Result<Option<std::path::PathBuf>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let mut header = [0u8; 64];
    if file.read(&mut header)? < 64 || &header[..4] != b"\x7fELF" {
        return Ok(None);
    }
    if header[4] != 2 || header[5] != 1 {
        return Err(Error::msg("Linux loader check requires a 64-bit little-endian ELF binary"));
    }
    let u64_at = |bytes: &[u8], offset| u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    let offset = u64_at(&header, 32);
    let size = u16::from_le_bytes(header[54..56].try_into().unwrap()) as u64;
    let count = u16::from_le_bytes(header[56..58].try_into().unwrap()) as u64;
    if size < 56 || count > 4096 {
        return Err(Error::msg("invalid ELF program headers"));
    }
    for index in 0..count {
        let at = index
            .checked_mul(size)
            .and_then(|value| offset.checked_add(value))
            .ok_or_else(|| Error::msg("invalid ELF offset"))?;
        file.seek(SeekFrom::Start(at))?;
        let mut program = [0u8; 56];
        file.read_exact(&mut program)?;
        if u32::from_le_bytes(program[..4].try_into().unwrap()) == 3 {
            let length = u64_at(&program, 32);
            if length == 0 || length > 4096 {
                return Err(Error::msg("invalid ELF interpreter size"));
            }
            file.seek(SeekFrom::Start(u64_at(&program, 8)))?;
            let mut name = vec![0u8; length as usize];
            file.read_exact(&mut name)?;
            let name = std::str::from_utf8(&name)
                .map_err(|_| Error::msg("invalid ELF interpreter path"))?
                .trim_end_matches('\0');
            return Ok(Some(name.into()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD_OS: &str = "./llama-server: /lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.38' not found (required by ./libllama-server-impl.so)\n./llama-server: /lib/x86_64-linux-gnu/libstdc++.so.6: version 'GLIBCXX_3.4.32' not found (required by ./libllama-server-impl.so)\n";
    const MISSING: &str = "linux-vdso.so.1 (0x00007fff)\nlibgomp.so.1 => not found\nlibssl.so.3 => not found\nlibcrypto.so.3 => not found\nlibstdc++.so.6 => not found\nlibcuda.so.1 => not found\n";
    const CLEAN: &str = "linux-vdso.so.1 (0x00007fff)\nlibgomp.so.1 => /usr/lib/libgomp.so.1 (0x00007f00)\nlibllama-server-impl.so => /cache/runtime/libllama-server-impl.so (0x00007f01)\n/lib64/ld-linux-x86-64.so.2 (0x00007f02)\n";

    #[test]
    fn loader_samples_and_distribution_fixes() {
        let old = loader_output(OLD_OS, "ID=ubuntu\nVERSION_ID=22.04");
        assert_eq!(old.len(), 2);
        assert!(old.iter().all(|check| check.message.contains("this OS is too old")));
        assert!(old.iter().any(|check| check.message.contains("GLIBC_2.38")));
        assert!(old.iter().any(|check| check.message.contains("GLIBCXX_3.4.32")));
        let missing = loader_output(MISSING, "ID=ubuntu\nVERSION_ID=\"24.04\"\nID_LIKE=debian");
        assert_eq!(missing.len(), 5);
        assert!(missing.iter().any(|check| check.message.contains("apt-get install libgomp1")));
        assert!(missing.iter().any(|check| check.message.contains("apt-get install libssl3t64")));
        assert!(missing.iter().any(|check| check.message.contains("GPU into the container")));
        assert!(loader_output(CLEAN, "ID=ubuntu").is_empty());
        assert_eq!(install_hint("libgomp.so.1", "ID=fedora"), "sudo dnf install libgomp");
        assert_eq!(install_hint("libssl.so.3", "ID=fedora"), "sudo dnf install openssl-libs");
        assert_eq!(install_hint("libstdc++.so.6", "ID=arch"), "sudo pacman -S gcc-libs");
        assert_eq!(install_hint("libgomp.so.1", "ID=derivative\nID_LIKE=\"rhel fedora\""), "sudo dnf install libgomp");
        assert_eq!(install_hint("libcrypto.so.3", "ID=debian\nVERSION_ID=12"), "sudo apt-get install libssl3");
        assert_eq!(install_hint("libcrypto.so.3", "ID=debian\nVERSION_ID=13"), "sudo apt-get install libssl3t64");
        assert!(install_hint("libgomp.so.1", "ID=unknown").contains("package manager"));
        assert!(install_hint("libmissing.so", "ID=ubuntu").contains("package providing libmissing.so"));
        let direct = loader_output(
            "llama-server: error while loading shared libraries: libgomp.so.1: cannot open shared object file: No such file or directory",
            "ID=ubuntu",
        );
        assert_eq!(direct.len(), 1);
        assert!(direct[0].message.contains("libgomp1"));
    }

    #[test]
    fn arm_prerequisites_use_the_measured_glibc_floor_and_cpu_packages() {
        assert_eq!(glibc("2.38").status, "ok");
        assert_eq!(glibc("2.43").status, "ok");
        assert_eq!(glibc("2.37").status, "fail");
        assert_eq!(glibc("unknown").status, "fail");
        assert_eq!(install_hint("libgomp.so.1", "ID=fedora"), "sudo dnf install libgomp");
        assert_eq!(install_hint("libgomp.so.1", "ID=ubuntu"), "sudo apt-get install libgomp1");
        assert_eq!(install_hint("libssl.so.3", "ID=fedora"), "sudo dnf install openssl-libs");
        assert_eq!(install_hint("libssl.so.3", "ID=ubuntu\nVERSION_ID=24.04"), "sudo apt-get install libssl3t64");
    }

    #[test]
    fn dotted_driver_versions_compare_numerically() {
        for version in ["570.26", "570.26.0", "570.124.06", "615.71.09"] {
            assert_eq!(driver(version, "570.26").status, "ok");
        }
        for version in ["570.9", "569.99.99", "unknown", "570.x"] {
            assert_eq!(driver(version, "570.26").status, "fail");
        }
        assert_eq!(driver("615.1", "615").status, "ok");
        assert_eq!(driver("614.99", "615").status, "fail");
    }

    #[test]
    fn disk_math_accounts_for_cached_and_partial_files() {
        assert_eq!(disk_bytes(0, 0), 0);
        assert_eq!(disk_bytes(100, 200), 900 + DISK_RESERVE);
        assert_eq!(disk_bytes(u64::MAX, 200), u64::MAX);
        assert_eq!(disk(Path::new("/cache"), 100, 100).status, "ok");
        assert_eq!(disk(Path::new("/cache"), 100, 99).status, "fail");
        let root = std::env::temp_dir().join(format!("ai-pool-checks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("weights.gguf");
        assert_eq!(download_bytes(&path, 100), 100);
        let part = path.with_file_name("weights.gguf.part");
        std::fs::write(&part, [0u8; 40]).unwrap();
        assert_eq!(download_bytes(&path, 100), 60);
        std::fs::write(&part, [0u8; 101]).unwrap();
        assert_eq!(download_bytes(&path, 100), 100);
        std::fs::write(&path, [0u8; 100]).unwrap();
        assert_eq!(download_bytes(&path, 100), 0);
        assert!(free_bytes(&root).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_the_elf_interpreter_without_executing_the_binary() {
        let path = std::env::temp_dir().join(format!("ai-pool-elf-{}", uuid::Uuid::new_v4()));
        let loader = b"/lib64/ld-linux-x86-64.so.2\0";
        let mut elf = vec![0u8; 120];
        elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
        elf[32..40].copy_from_slice(&64u64.to_le_bytes());
        elf[54..56].copy_from_slice(&56u16.to_le_bytes());
        elf[56..58].copy_from_slice(&1u16.to_le_bytes());
        elf[64..68].copy_from_slice(&3u32.to_le_bytes());
        elf[72..80].copy_from_slice(&120u64.to_le_bytes());
        elf[96..104].copy_from_slice(&(loader.len() as u64).to_le_bytes());
        elf.extend_from_slice(loader);
        std::fs::write(&path, &elf).unwrap();
        assert_eq!(interpreter(&path).unwrap(), Some("/lib64/ld-linux-x86-64.so.2".into()));
        elf[56..58].copy_from_slice(&0u16.to_le_bytes());
        std::fs::write(&path, &elf).unwrap();
        assert!(interpreter(&path).unwrap().is_none());
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
        assert!(interpreter(&path).unwrap().is_none());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn launcher_and_loader_prepend_the_same_bundle_directory() {
        if !cfg!(target_os = "linux") {
            return;
        }
        let path = std::env::temp_dir().join(format!("ai-pool-library-path-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"fixture").unwrap();
        let mut command = tokio::process::Command::new(&path);
        library_path(&mut command, &path).unwrap();
        let (_, value) = command.as_std().get_envs().find(|(key, _)| *key == "LD_LIBRARY_PATH").unwrap();
        let first = std::env::split_paths(value.unwrap()).next().unwrap();
        assert_eq!(first, std::fs::canonicalize(&path).unwrap().parent().unwrap());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn doctor_fixture_output() {
        println!("Failing Ubuntu 22.04 fixture:");
        for check in loader_output(OLD_OS, "ID=ubuntu\nVERSION_ID=22.04")
            .into_iter()
            .chain([driver("535.104.05", "570.26"), disk(Path::new("/cache/ai-pool"), 12_800_000_000, 6_000_000_000)])
        {
            println!("{}", check.line());
        }
        println!("Minimal Ubuntu 24.04 fixture:");
        for check in loader_output("libgomp.so.1 => not found", "ID=ubuntu\nVERSION_ID=24.04") {
            println!("{}", check.line());
        }
        println!("Passing fixture:");
        for check in [
            Check::ok("Linux loader", "runtime and bundled libraries resolve; required symbol versions are available"),
            driver("570.124.06", "570.26"),
            disk(Path::new("/cache/ai-pool"), 12_800_000_000, 30_000_000_000),
        ] {
            println!("{}", check.line());
        }
    }
}

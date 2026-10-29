//! Accelerator detection. Unknown memory is an error, never "unlimited".

#[cfg(any(test, all(target_os = "linux", target_arch = "aarch64")))]
mod cpu;

use std::process::Command;

use pool_protocol::{
    Accelerator,
    messages::{Device, Hardware},
};

/// Safety margin kept free on a dedicated GPU beyond the profile figures.
#[cfg(not(all(target_os = "linux", target_arch = "aarch64")))]
const CUDA_MARGIN_MIB: u64 = 256;
/// Share of unified memory a Metal miner may plan to use (an estimate).
const UNIFIED_SHARE: f64 = 0.70;
/// System RAM left for the OS and other programs by CPU and offload profiles.
const RAM_MARGIN_MIB: u64 = 2048;

/// Memory the OS could give out now, including reclaimable cache.
#[cfg(not(all(target_os = "linux", target_arch = "aarch64")))]
fn available_ram_mib() -> u64 {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    system.available_memory() / (1 << 20)
}

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub index: u32,
    pub name: String,
    pub total_mib: u64,
    pub free_mib: u64,
    pub driver: String,
}

#[derive(Clone, Debug)]
pub struct Machine {
    pub accelerator: Accelerator,
    pub devices: Vec<DeviceInfo>,
    /// The compute device this miner serves from.
    pub selected: usize,
    /// Memory the profile plan may use on the selected device.
    pub budget_mib: u64,
    pub budget_note: String,
    /// System RAM offload profiles may use for weights kept on the host;
    /// 0 where a separate host reservation does not apply (CPU/unified memory).
    pub ram_budget_mib: u64,
    /// CPU worker threads selected from the host topology (ARM Linux only).
    pub cpu_threads: Option<usize>,
}

impl Machine {
    pub fn device(&self) -> &DeviceInfo {
        &self.devices[self.selected]
    }

    /// Value for `CUDA_VISIBLE_DEVICES`, when relevant.
    pub fn cuda_device(&self) -> Option<String> {
        (self.accelerator == Accelerator::Cuda).then(|| self.device().index.to_string())
    }

    pub fn device_id(&self) -> String {
        format!("{}{}", if self.accelerator == Accelerator::Cpu { "cpu" } else { "gpu" }, self.device().index)
    }

    pub fn hardware(&self) -> Hardware {
        let device = self.device();
        Hardware {
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            accelerator: self.accelerator,
            devices: vec![Device {
                id: self.device_id(),
                name: device.name.clone(),
                memory_total_mib: device.total_mib,
            }],
        }
    }

    pub fn describe(&self) -> String {
        let device = self.device();
        let driver = if device.driver.is_empty() { String::new() } else { format!(", driver {}", device.driver) };
        let ram = if self.ram_budget_mib > 0 {
            format!("; system RAM for offloading {} MiB (available minus {RAM_MARGIN_MIB} MiB)", self.ram_budget_mib)
        } else {
            String::new()
        };
        format!(
            "{} {} ({} MiB total, {} MiB free{driver}); planning budget {} MiB ({}){ram}",
            self.accelerator, device.name, device.total_mib, device.free_mib, self.budget_mib, self.budget_note
        )
    }
}

pub fn detect(device: Option<u32>) -> Result<Machine, String> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        return detect_apple();
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    return cpu::detect(device);
    #[cfg(not(all(target_os = "linux", target_arch = "aarch64")))]
    detect_nvidia(device)
}

#[cfg(not(all(target_os = "linux", target_arch = "aarch64")))]
fn detect_nvidia(device: Option<u32>) -> Result<Machine, String> {
    let output = Command::new("nvidia-smi")
        .args(["--query-gpu=index,name,memory.total,memory.free,driver_version", "--format=csv,noheader,nounits"])
        .output()
        .map_err(|error| {
            format!("no supported accelerator: nvidia-smi could not run ({error}); install the host NVIDIA driver, or pass the GPU into the container with NVIDIA Container Toolkit")
        })?;
    if !output.status.success() {
        return Err(format!(
            "nvidia-smi failed ({}): {}; check the host NVIDIA driver and GPU access in the container",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let gpus = parse_nvidia_smi(&String::from_utf8_lossy(&output.stdout))?;
    let selected = match device {
        None => 0,
        Some(index) => gpus
            .iter()
            .position(|gpu| gpu.index == index)
            .ok_or_else(|| format!("--device {index}: no such GPU (found {})", gpus.len()))?,
    };
    let budget_mib = gpus[selected].free_mib.saturating_sub(CUDA_MARGIN_MIB);
    Ok(Machine {
        accelerator: Accelerator::Cuda,
        devices: gpus,
        selected,
        budget_mib,
        budget_note: format!("free memory minus {CUDA_MARGIN_MIB} MiB"),
        ram_budget_mib: available_ram_mib().saturating_sub(RAM_MARGIN_MIB),
        cpu_threads: None,
    })
}

#[cfg(any(test, not(all(target_os = "linux", target_arch = "aarch64"))))]
fn parse_nvidia_smi(text: &str) -> Result<Vec<DeviceInfo>, String> {
    let mut gpus = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<_> = line.split(',').map(str::trim).collect();
        let [index, name, total, free, driver] = fields.as_slice() else {
            return Err(format!("unexpected nvidia-smi output: {line:?}"));
        };
        let number = |value: &str, what: &str| {
            value
                .parse::<u64>()
                .map_err(|_| format!("nvidia-smi reported no usable {what} ({value:?}) for GPU {index}"))
        };
        gpus.push(DeviceInfo {
            index: number(index, "index")? as u32,
            name: name.to_string(),
            total_mib: number(total, "total memory")?,
            free_mib: number(free, "free memory")?,
            driver: driver.to_string(),
        });
    }
    if gpus.is_empty() {
        return Err("nvidia-smi found no GPUs".into());
    }
    Ok(gpus)
}

fn detect_apple() -> Result<Machine, String> {
    let sysctl = |key: &str| -> Result<String, String> {
        let output = Command::new("sysctl").args(["-n", key]).output().map_err(|e| format!("sysctl failed: {e}"))?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    let bytes: u64 = sysctl("hw.memsize")?.parse().map_err(|_| "cannot read hw.memsize".to_string())?;
    let total_mib = bytes >> 20;
    let name = sysctl("machdep.cpu.brand_string").unwrap_or_default();
    let budget_mib = (total_mib as f64 * UNIFIED_SHARE) as u64;
    Ok(Machine {
        accelerator: Accelerator::Metal,
        devices: vec![DeviceInfo {
            index: 0,
            name: if name.is_empty() { "Apple Silicon".into() } else { name },
            total_mib,
            free_mib: total_mib,
            driver: String::new(),
        }],
        selected: 0,
        budget_mib,
        budget_note: format!("estimate: {:.0}% of unified memory", UNIFIED_SHARE * 100.0),
        ram_budget_mib: 0,
        cpu_threads: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nvidia_smi() {
        let gpus = parse_nvidia_smi("0, NVIDIA GeForce RTX 5070 Ti, 16303, 14888, 615.71.09\n").unwrap();
        assert_eq!(gpus[0].free_mib, 14888);
        assert!(parse_nvidia_smi("0, GPU, [N/A], 1, x").is_err());
    }
}

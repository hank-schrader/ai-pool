//! ARM Linux uses the CPU backend; its memory budget is available system RAM.

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
use super::{Accelerator, DeviceInfo, Machine, RAM_MARGIN_MIB};

/// Heterogeneous cores expose a capacity or maximum clock for each online CPU.
/// Uniform hosts (including Ampere/Graviton) use every available core. Missing
/// topology also falls back to the OS's available parallelism.
fn performance_threads(capacities: Option<&[u64]>, frequencies: Option<&[u64]>, available: usize) -> usize {
    let performance = |values: Option<&[u64]>| {
        let values = values.filter(|values| values.iter().all(|&value| value > 0))?;
        let highest = values.iter().max()?;
        let count = values.iter().filter(|value| *value == highest).count();
        (count < values.len()).then_some(count)
    };
    performance(capacities).or_else(|| performance(frequencies)).unwrap_or(available).min(available).max(1)
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
pub fn detect(device: Option<u32>) -> Result<Machine, String> {
    use std::{fs, path::Path};

    if device.is_some_and(|index| index != 0) {
        return Err("ARM Linux has one CPU device; use --device 0 or omit --device".into());
    }
    let available = std::thread::available_parallelism().map_err(|e| format!("cannot count CPU threads: {e}"))?.get();
    let root = Path::new("/sys/devices/system/cpu");
    let cores: Vec<_> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.strip_prefix("cpu").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            })
        })
        .filter(|path| !fs::read_to_string(path.join("online")).is_ok_and(|value| value.trim() == "0"))
        .collect();
    let metrics = |name: &str| -> Option<Vec<u64>> {
        if cores.is_empty() {
            return None;
        }
        cores.iter().map(|core| fs::read_to_string(core.join(name)).ok()?.trim().parse().ok()).collect()
    };
    let capacities = metrics("cpu_capacity");
    let frequencies = metrics("cpufreq/cpuinfo_max_freq");
    let threads = performance_threads(capacities.as_deref(), frequencies.as_deref(), available);
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let total_mib = system.total_memory() >> 20;
    let free_mib = system.available_memory() >> 20;
    if total_mib == 0 || free_mib == 0 {
        return Err("cannot read usable system RAM from /proc/meminfo".into());
    }
    let name = fs::read_to_string("/proc/device-tree/model").unwrap_or_else(|_| "ARM CPU".into());
    Ok(Machine {
        accelerator: Accelerator::Cpu,
        devices: vec![DeviceInfo {
            index: 0,
            name: name.trim_matches(['\0', '\n']).into(),
            total_mib,
            free_mib,
            driver: String::new(),
        }],
        selected: 0,
        budget_mib: free_mib.saturating_sub(RAM_MARGIN_MIB),
        budget_note: format!("available RAM minus {RAM_MARGIN_MIB} MiB; {threads}/{available} CPU threads"),
        ram_budget_mib: 0,
        cpu_threads: Some(threads),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_performance_cores_and_supports_uniform_or_unknown_hosts() {
        assert_eq!(performance_threads(Some(&[485, 485, 1024, 1024, 1024, 1024, 1024, 1024]), None, 8), 6);
        assert_eq!(performance_threads(None, Some(&[2064, 2064, 3036, 3036, 3036, 3036, 3036, 3036]), 8), 6);
        assert_eq!(
            performance_threads(Some(&[1024; 8]), Some(&[2064, 2064, 3036, 3036, 3036, 3036, 3036, 3036]), 8),
            6
        );
        assert_eq!(performance_threads(Some(&[1024; 64]), None, 64), 64);
        assert_eq!(performance_threads(None, Some(&[3000; 16]), 16), 16);
        assert_eq!(performance_threads(None, None, 8), 8);
        assert_eq!(performance_threads(Some(&[0; 8]), None, 8), 8);
        assert_eq!(performance_threads(Some(&[1024; 64]), None, 4), 4);
    }
}

pub const MIN_RAM_MB: u32 = 1024;
pub const AUTO_RAM_MAX_MB: u32 = 10 * 1024;
pub const RAM_STEP_MB: u32 = 512;

/// Returns the amount of physically installed memory in MiB.
pub fn total_memory_mb() -> Option<u32> {
    total_memory_kib().and_then(|kib| {
        let mib = kib / 1024;
        (mib > 0).then(|| mib.min(u32::MAX as u64) as u32)
    })
}

/// Automatic allocation uses half of physical RAM, capped at 10 GiB.
pub fn automatic_ram_mb(total_memory_mb: u32) -> u32 {
    (total_memory_mb / 2).min(AUTO_RAM_MAX_MB)
}

/// The manual slider follows installed RAM instead of imposing an arbitrary
/// launcher limit. Its value is aligned to the slider step.
pub fn manual_ram_max_mb(total_memory_mb: u32) -> u32 {
    let aligned = total_memory_mb / RAM_STEP_MB * RAM_STEP_MB;
    aligned.max(MIN_RAM_MB)
}

#[cfg(target_os = "windows")]
fn total_memory_kib() -> Option<u64> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetPhysicallyInstalledSystemMemory(total_memory_in_kilobytes: *mut u64) -> i32;
    }

    let mut total_kib = 0_u64;
    // SAFETY: Windows writes one u64 to the valid out pointer. The call does
    // not retain the pointer after it returns.
    let succeeded = unsafe { GetPhysicallyInstalledSystemMemory(&mut total_kib) };
    (succeeded != 0 && total_kib > 0).then_some(total_kib)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn total_memory_kib() -> Option<u64> {
    let meminfo = std::fs::read_to_string(std::path::Path::new("/proc/meminfo")).ok()?;
    let value = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?
        .split_whitespace()
        .next()?;
    value.parse().ok()
}

#[cfg(target_os = "macos")]
fn total_memory_kib() -> Option<u64> {
    let output = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let bytes = std::str::from_utf8(&output.stdout)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(bytes / 1024)
}

#[cfg(not(any(
    target_os = "windows",
    target_os = "linux",
    target_os = "android",
    target_os = "macos"
)))]
fn total_memory_kib() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_ram_is_half_of_total_memory() {
        assert_eq!(automatic_ram_mb(1024), 512);
        assert_eq!(automatic_ram_mb(8 * 1024), 4 * 1024);
        assert_eq!(automatic_ram_mb(16 * 1024), 8 * 1024);
    }

    #[test]
    fn automatic_ram_never_exceeds_ten_gibibytes() {
        assert_eq!(automatic_ram_mb(24 * 1024), AUTO_RAM_MAX_MB);
        assert_eq!(automatic_ram_mb(64 * 1024), AUTO_RAM_MAX_MB);
    }

    #[test]
    fn manual_limit_uses_all_installed_memory_without_a_fixed_cap() {
        assert_eq!(manual_ram_max_mb(32 * 1024), 32 * 1024);
        assert_eq!(manual_ram_max_mb(128 * 1024), 128 * 1024);
    }

    #[test]
    #[cfg(any(
        target_os = "windows",
        target_os = "linux",
        target_os = "android",
        target_os = "macos"
    ))]
    fn detects_physical_memory_on_supported_platforms() {
        assert!(total_memory_mb().is_some_and(|total| total >= 1024));
    }
}

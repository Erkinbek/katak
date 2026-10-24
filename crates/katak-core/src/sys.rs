//! Small OS queries used for diagnostics and benchmarks.

/// Private (non-file-backed) memory of this process in bytes: the commit
/// charge on Windows, anonymous resident memory on Linux. File-backed pages
/// of memory maps are not included.
pub fn private_bytes() -> Option<u64> {
    imp::private_bytes()
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    pub fn private_bytes() -> Option<u64> {
        let mut c: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        // SAFETY: `c` is a properly sized, writable counters struct; the
        // EX layout starts with the basic one.
        let ok = unsafe {
            GetProcessMemoryInfo(
                GetCurrentProcess(),
                (&mut c as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
                c.cb,
            )
        };
        (ok != 0).then_some(c.PrivateUsage as u64)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    pub fn private_bytes() -> Option<u64> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|l| l.starts_with("RssAnon:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    pub fn private_bytes() -> Option<u64> {
        None
    }
}

/// Formats a byte count as `12.3 MB`.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn human() {
        assert_eq!(super::human_bytes(512), "512 B");
        assert_eq!(super::human_bytes(1536), "1.5 KB");
        assert_eq!(super::human_bytes(10 << 30), "10.0 GB");
    }

    #[test]
    #[cfg(any(windows, target_os = "linux"))]
    fn private_bytes_is_available() {
        assert!(super::private_bytes().unwrap() > 0);
    }
}

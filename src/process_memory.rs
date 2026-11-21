//! Process memory for the status bar.
//!
//! On macOS this reports `ri_phys_footprint` from the public libproc API, not
//! resident-set size, virtual size, peak usage, or the query-engine memory budget.
//! Footprint includes memory charged to the process, including compressed
//! memory; it need not equal a simultaneous Activity Monitor display.
//!
//! SDK contracts: <libproc.h> `proc_pid_rusage` (macOS 10.9+), and
//! <sys/resource.h> `RUSAGE_INFO_V0` / `struct rusage_info_v0`.
//! https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/resource.h
//! https://github.com/apple-oss-distributions/xnu/blob/main/libsyscall/wrappers/libproc/libproc.c
//!
/// Read the current physical footprint on the app's background executor.
pub fn sample() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        macos::sample()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// A compact, rounded IEC byte count, without a UI label. Failed samples display
/// an em dash; zero is a known zero and remains distinct from an unknown value.
pub fn format_bytes(bytes: Option<u64>) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let Some(bytes) = bytes else {
        return "—".into();
    };
    if bytes < 1024 {
        return format!("{bytes} B");
    }

    let bytes = u128::from(bytes);
    let mut unit = 1;
    let mut divisor = 1024_u128;
    while unit + 1 < UNITS.len() && bytes >= divisor * 1024 {
        divisor *= 1024;
        unit += 1;
    }
    // u128 avoids overflow and floating-point precision loss near u64::MAX.
    let mut tenths = (bytes * 10 + divisor / 2) / divisor;
    // Promote values that round to 1024.0, rather than showing 1024.0 KiB.
    if tenths >= 10_240 && unit + 1 < UNITS.len() {
        divisor *= 1024;
        unit += 1;
        tenths = (bytes * 10 + divisor / 2) / divisor;
    }
    format!("{}.{:01} {}", tenths / 10, tenths % 10, UNITS[unit])
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::c_void;

    const RUSAGE_INFO_V0: i32 = 0;

    /// The fixed V0 ABI is 16 UUID bytes followed by ten uint64_t fields.
    /// Requesting V0 keeps this allocation independent of newer V1–V6 fields.
    #[repr(C)]
    #[derive(Default)]
    struct RusageInfoV0 {
        uuid: [u8; 16],
        user_time: u64,
        system_time: u64,
        package_idle_wakeups: u64,
        interrupt_wakeups: u64,
        pageins: u64,
        wired_size: u64,
        resident_size: u64,
        physical_footprint: u64,
        process_start_abstime: u64,
        process_exit_abstime: u64,
    }

    #[link(name = "proc")]
    unsafe extern "C" {
        // The SDK typedefs rusage_info_t as void*, and passes its address type.
        // The actual argument is writable storage for the requested version,
        // as in Apple's C API; the function does not return an owned pointer.
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut *mut c_void) -> i32;
    }

    pub(super) fn sample() -> Option<u64> {
        let pid = i32::try_from(std::process::id()).ok()?;
        let mut info = RusageInfoV0::default();
        // SAFETY: RusageInfoV0 matches the SDK's complete V0 structure and is
        // zero-initialized, aligned, live and exclusively borrowed for this
        // synchronous call. The fixed V0 flavor writes exactly that layout.
        // libproc supports querying our own pid without a privileged task port
        // and does not retain or free the supplied storage. No thread-bound
        // AppKit object or global mutable Rust state is involved.
        let status = unsafe {
            proc_pid_rusage(
                pid,
                RUSAGE_INFO_V0,
                (&mut info as *mut RusageInfoV0).cast::<*mut c_void>(),
            )
        };
        (status == 0).then_some(info.physical_footprint)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn v0_layout_matches_the_public_sdk_abi() {
            assert_eq!(std::mem::size_of::<RusageInfoV0>(), 96);
            assert_eq!(std::mem::align_of::<RusageInfoV0>(), 8);
            assert_eq!(std::mem::offset_of!(RusageInfoV0, resident_size), 64);
            assert_eq!(std::mem::offset_of!(RusageInfoV0, physical_footprint), 72);
            assert_eq!(std::mem::offset_of!(RusageInfoV0, process_exit_abstime), 88);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_and_byte_values_remain_distinct() {
        assert_eq!(format_bytes(None), "—");
        assert_eq!(format_bytes(Some(0)), "0 B");
        assert_eq!(format_bytes(Some(1)), "1 B");
        assert_eq!(format_bytes(Some(1023)), "1023 B");
    }

    #[test]
    fn iec_units_round_at_boundaries_without_overflow() {
        for (bytes, expected) in [
            (1024, "1.0 KiB"),
            (1536, "1.5 KiB"),
            (1_048_575, "1.0 MiB"),
            (1_048_576, "1.0 MiB"),
            (180_436_992, "172.1 MiB"),
            (1_073_741_824, "1.0 GiB"),
            (1_099_511_627_776, "1.0 TiB"),
            (1_125_899_906_842_624, "1.0 PiB"),
            (1_152_921_504_606_846_976, "1.0 EiB"),
            (u64::MAX, "16.0 EiB"),
        ] {
            assert_eq!(format_bytes(Some(bytes)), expected, "{bytes} bytes");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reads_a_real_current_process_footprint() {
        let bytes = sample().expect("libproc should read this test process's footprint");
        assert!(bytes > 0, "a running process has a nonzero footprint");
        assert!(bytes < 1 << 60, "the API must report bytes, not a pointer");
        assert!(!format_bytes(Some(bytes)).contains('—'));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn unsupported_platform_does_not_substitute_rss() {
        assert_eq!(sample(), None);
        assert_eq!(format_bytes(sample()), "—");
    }
}

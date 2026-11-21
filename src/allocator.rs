//! Return unused Rust heap pages promptly when large Arrow buffers are dropped.

#[cfg(target_os = "macos")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub(crate) fn configure() {
    #[cfg(target_os = "macos")]
    {
        // The pinned libmimalloc-sys v3 header defines this as enum member 15.
        // Its Rust bindings expose mi_option_set but omit this option constant.
        const PURGE_DELAY: libmimalloc_sys::mi_option_t = 15;
        // SAFETY: PURGE_DELAY is a valid option in the pinned C library. Set it
        // at startup, before starting any GUI or data workers. Zero means purge
        // empty pages immediately instead of retaining them for a later allocation.
        unsafe { libmimalloc_sys::mi_option_set(PURGE_DELAY, 0) };
    }
}

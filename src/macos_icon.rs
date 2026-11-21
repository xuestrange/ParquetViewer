//! Install the embedded application artwork for bundled and direct launches.
//! Call from GPUI's application startup callback on the main thread.
//!
//! Signatures and ownership follow the local macOS SDK's NSThread.h,
//! NSData.h, NSImage.h and NSApplication.h. No filesystem or icon-cache writes.

// objc 0.2's selector macro checks this legacy cfg in the consuming crate.
#![allow(unexpected_cfgs)]

use anyhow::{Result, anyhow, ensure};
use objc::{
    msg_send,
    rc::{StrongPtr, autoreleasepool},
    runtime::{BOOL, Class, Object, YES},
    sel, sel_impl,
};

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {}

pub(crate) fn install() -> Result<()> {
    const PNG: &[u8] = include_bytes!("../packaging/assets/app-icon-1024.png");

    autoreleasepool(|| {
        let thread_class =
            Class::get("NSThread").ok_or_else(|| anyhow!("macOS Foundation is unavailable."))?;
        let data_class =
            Class::get("NSData").ok_or_else(|| anyhow!("macOS Foundation is unavailable."))?;
        let image_class =
            Class::get("NSImage").ok_or_else(|| anyhow!("macOS AppKit is unavailable."))?;
        let application_class =
            Class::get("NSApplication").ok_or_else(|| anyhow!("macOS AppKit is unavailable."))?;

        // SAFETY: Selectors and ABI types match the SDK declarations. AppKit
        // access occurs only after checking NSThread.isMainThread. Each init
        // consumes its allocation and returns +1 (or nil); StrongPtr adopts
        // that ownership without another retain and releases on every exit.
        unsafe {
            let is_main: BOOL = msg_send![thread_class, isMainThread];
            ensure!(
                is_main == YES,
                "The application icon must be installed on the main thread."
            );

            let application: *mut Object = msg_send![application_class, sharedApplication];
            ensure!(
                !application.is_null(),
                "The macOS application is unavailable."
            );

            let data_allocated: *mut Object = msg_send![data_class, alloc];
            ensure!(
                !data_allocated.is_null(),
                "Could not allocate application icon data."
            );
            let data: *mut Object = msg_send![data_allocated,
                initWithBytes: PNG.as_ptr().cast::<std::ffi::c_void>() length: PNG.len()
            ];
            ensure!(
                !data.is_null(),
                "Could not copy the embedded application icon."
            );
            let data = StrongPtr::new(data);

            let image_allocated: *mut Object = msg_send![image_class, alloc];
            ensure!(
                !image_allocated.is_null(),
                "Could not allocate the application icon image."
            );
            let image: *mut Object = msg_send![image_allocated, initWithData: *data];
            ensure!(
                !image.is_null(),
                "Could not decode the embedded application icon."
            );
            let image = StrongPtr::new(image);
            let is_valid: BOOL = msg_send![*image, isValid];
            ensure!(
                is_valid == YES,
                "The embedded application icon is not a valid image."
            );

            // The SDK declares applicationIconImage as strong. NSApplication
            // retains the image before our local image/data owners are dropped.
            let _: () = msg_send![application, setApplicationIconImage: *image];
        }
        Ok(())
    })
}

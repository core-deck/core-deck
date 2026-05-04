//! macOS IOKit-based USB hotplug detection
//!
//! Uses IOKit notifications for instant device arrival/removal detection.

use core_foundation::base::TCFType;
use core_foundation::number::CFNumber;
use core_foundation::runloop::{kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopRunInMode};
use core_foundation::string::CFString;
use core_foundation_sys::dictionary::CFDictionarySetValue;
use core_foundation_sys::runloop::{
    CFRunLoopAddSource, CFRunLoopGetCurrent, CFRunLoopSourceRef, CFRunLoopStop,
    kCFRunLoopRunFinished, kCFRunLoopRunStopped,
};
use io_kit_sys::*;
use io_kit_sys::types::io_iterator_t;
use mach2::port::MACH_PORT_NULL;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

/// Event type for hotplug notifications
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotplugEvent {
    /// Device was connected
    DeviceArrived,
    /// Device was removed
    DeviceRemoved,
}

/// Hotplug watcher for USB devices on macOS
pub struct HotplugWatcher {
    /// Whether the watcher should stop
    stop: Arc<AtomicBool>,
    /// Handle to the watcher thread
    thread_handle: Option<thread::JoinHandle<()>>,
    /// Run loop reference for stopping
    run_loop: Arc<parking_lot::Mutex<Option<CFRunLoop>>>,
}

/// Context passed to IOKit callbacks
struct CallbackContext {
    event_tx: mpsc::UnboundedSender<HotplugEvent>,
}

impl HotplugWatcher {
    /// Create a new hotplug watcher for the given USB VID/PID
    pub fn new(
        vendor_id: u16,
        product_id: u16,
        event_tx: mpsc::UnboundedSender<HotplugEvent>,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let run_loop = Arc::new(parking_lot::Mutex::new(None));
        let run_loop_clone = Arc::clone(&run_loop);

        let thread_handle = thread::spawn(move || {
            if let Err(e) = run_watcher(vendor_id, product_id, event_tx, stop_clone, run_loop_clone)
            {
                error!("Hotplug watcher error: {}", e);
            }
        });

        Ok(Self {
            stop,
            thread_handle: Some(thread_handle),
            run_loop,
        })
    }

    /// Stop the hotplug watcher
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);

        // Wake up the run loop so it can exit
        if let Some(rl) = self.run_loop.lock().take() {
            unsafe {
                CFRunLoopStop(rl.as_concrete_TypeRef());
            }
        }

        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for HotplugWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Run the IOKit notification watcher
fn run_watcher(
    vendor_id: u16,
    product_id: u16,
    event_tx: mpsc::UnboundedSender<HotplugEvent>,
    stop: Arc<AtomicBool>,
    run_loop_storage: Arc<parking_lot::Mutex<Option<CFRunLoop>>>,
) -> Result<(), String> {
    unsafe {
        // Create a master port for IOKit
        let mut master_port: mach2::port::mach_port_t = MACH_PORT_NULL;
        let kr = IOMasterPort(MACH_PORT_NULL, &mut master_port);
        if kr != 0 {
            return Err(format!("IOMasterPort failed: {}", kr));
        }

        // Create notification port
        let notify_port = IONotificationPortCreate(master_port);
        if notify_port.is_null() {
            return Err("IONotificationPortCreate failed".to_string());
        }

        // Get the run loop source from the notification port
        let run_loop_source: CFRunLoopSourceRef = IONotificationPortGetRunLoopSource(notify_port);
        if run_loop_source.is_null() {
            IONotificationPortDestroy(notify_port);
            return Err("IONotificationPortGetRunLoopSource failed".to_string());
        }

        // Get current run loop and add source
        let run_loop = CFRunLoopGetCurrent();
        CFRunLoopAddSource(run_loop, run_loop_source, kCFRunLoopDefaultMode);

        // Store run loop reference for stopping
        {
            let rl = CFRunLoop::wrap_under_get_rule(run_loop);
            *run_loop_storage.lock() = Some(rl);
        }

        // Create matching dictionary for USB devices
        let matching_dict = IOServiceMatching(kIOUSBDeviceClassName.as_ptr() as *const i8);
        if matching_dict.is_null() {
            IONotificationPortDestroy(notify_port);
            return Err("IOServiceMatching failed".to_string());
        }

        // Add vendor ID to matching dictionary
        let vendor_key = CFString::new("idVendor");
        let vendor_num = CFNumber::from(vendor_id as i32);
        CFDictionarySetValue(
            matching_dict,
            vendor_key.as_concrete_TypeRef() as *const c_void,
            vendor_num.as_concrete_TypeRef() as *const c_void,
        );

        // Add product ID to matching dictionary
        let product_key = CFString::new("idProduct");
        let product_num = CFNumber::from(product_id as i32);
        CFDictionarySetValue(
            matching_dict,
            product_key.as_concrete_TypeRef() as *const c_void,
            product_num.as_concrete_TypeRef() as *const c_void,
        );

        // Create context for callbacks - leaked to keep alive for callbacks
        let arrival_ctx = Box::leak(Box::new(CallbackContext {
            event_tx: event_tx.clone(),
        }));
        let removal_ctx = Box::leak(Box::new(CallbackContext { event_tx }));

        // Register for device arrival notifications
        // We need to retain the matching dict since IOServiceAddMatchingNotification consumes it
        CFRetain(matching_dict as *const c_void);

        let mut arrival_iterator: io_iterator_t = 0;
        let kr = IOServiceAddMatchingNotification(
            notify_port,
            kIOMatchedNotification.as_ptr() as *mut i8,
            matching_dict,
            device_arrived_callback,
            arrival_ctx as *mut CallbackContext as *mut c_void,
            &mut arrival_iterator,
        );
        if kr != 0 {
            IONotificationPortDestroy(notify_port);
            return Err(format!(
                "IOServiceAddMatchingNotification (arrival) failed: {}",
                kr
            ));
        }

        // Drain the iterator to arm the notification (and check if device is already connected)
        drain_iterator(arrival_iterator, HotplugEvent::DeviceArrived, true);

        // Register for device removal notifications
        let mut removal_iterator: io_iterator_t = 0;
        let kr = IOServiceAddMatchingNotification(
            notify_port,
            kIOTerminatedNotification.as_ptr() as *mut i8,
            matching_dict,
            device_removed_callback,
            removal_ctx as *mut CallbackContext as *mut c_void,
            &mut removal_iterator,
        );
        if kr != 0 {
            IOObjectRelease(arrival_iterator);
            IONotificationPortDestroy(notify_port);
            return Err(format!(
                "IOServiceAddMatchingNotification (removal) failed: {}",
                kr
            ));
        }

        // Drain the iterator to arm the notification
        drain_iterator(removal_iterator, HotplugEvent::DeviceRemoved, false);

        info!(
            "IOKit hotplug watcher started for VID:0x{:04X} PID:0x{:04X}",
            vendor_id, product_id
        );

        // Run the event loop
        while !stop.load(Ordering::SeqCst) {
            let result = CFRunLoopRunInMode(kCFRunLoopDefaultMode, 1.0, 0);
            if result == kCFRunLoopRunStopped || result == kCFRunLoopRunFinished {
                break;
            }
        }

        info!("IOKit hotplug watcher stopped");

        // Cleanup
        IOObjectRelease(arrival_iterator);
        IOObjectRelease(removal_iterator);
        IONotificationPortDestroy(notify_port);
    }

    Ok(())
}

/// Drain an iterator to arm notifications
unsafe fn drain_iterator(iterator: io_iterator_t, event: HotplugEvent, log_existing: bool) {
    loop {
        let service = IOIteratorNext(iterator);
        if service == 0 {
            break;
        }
        if log_existing {
            debug!("Existing device found during init: {:?}", event);
        }
        IOObjectRelease(service);
    }
}

/// Callback for device arrival
unsafe extern "C" fn device_arrived_callback(refcon: *mut c_void, iterator: io_iterator_t) {
    let ctx = &*(refcon as *const CallbackContext);

    // Drain the iterator (required to re-arm the notification)
    loop {
        let service = IOIteratorNext(iterator);
        if service == 0 {
            break;
        }
        info!("USB device arrived");
        let _ = ctx.event_tx.send(HotplugEvent::DeviceArrived);
        IOObjectRelease(service);
    }
}

/// Callback for device removal
unsafe extern "C" fn device_removed_callback(refcon: *mut c_void, iterator: io_iterator_t) {
    let ctx = &*(refcon as *const CallbackContext);

    // Drain the iterator (required to re-arm the notification)
    loop {
        let service = IOIteratorNext(iterator);
        if service == 0 {
            break;
        }
        info!("USB device removed");
        let _ = ctx.event_tx.send(HotplugEvent::DeviceRemoved);
        IOObjectRelease(service);
    }
}

// IOKit constants not in io-kit-sys (using Apple's naming convention)
#[allow(non_upper_case_globals)]
const kIOUSBDeviceClassName: &str = "IOUSBDevice\0";
#[allow(non_upper_case_globals)]
const kIOMatchedNotification: &str = "IOServiceMatched\0";
#[allow(non_upper_case_globals)]
const kIOTerminatedNotification: &str = "IOServiceTerminate\0";

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRetain(cf: *const c_void);
}

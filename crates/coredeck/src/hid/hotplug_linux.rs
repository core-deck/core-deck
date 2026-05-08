//! Linux udev-based USB hotplug detection.
//!
//! Counterpart to `hotplug_macos`'s IOKit watcher. We subscribe to
//! the kernel's `usb` subsystem via netlink (libudev's monitor API),
//! filter events down to our VID/PID, and forward `DeviceArrived` /
//! `DeviceRemoved` to the device manager. No polling — the thread
//! sleeps on `poll(2)` against the udev socket and wakes only when
//! the kernel publishes an event.
//!
//! Why not the polling fallback: 500ms-to-5s plug-detection latency
//! is fine functionally, but the right Linux primitive here is udev,
//! and we already require libudev for hidapi anyway. Same shape as
//! `hotplug_macos.rs` so the device manager picks one or the other
//! purely on `cfg(target_os = …)`.

use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Event type for hotplug notifications. Mirrors the macOS variant
/// so the device manager's match arms don't have to care about which
/// backend produced the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotplugEvent {
    DeviceArrived,
    DeviceRemoved,
}

/// Hotplug watcher for USB devices on Linux.
///
/// Owns a worker thread that blocks on `poll(2)` against the udev
/// monitor's netlink socket and forwards matching events on `event_tx`.
/// Drop calls `stop()` to break the loop and join.
pub struct HotplugWatcher {
    stop: Arc<AtomicBool>,
    thread_handle: Option<thread::JoinHandle<()>>,
}

impl HotplugWatcher {
    /// Create a new hotplug watcher for the given USB VID/PID.
    ///
    /// The udev monitor is built inside the worker thread because
    /// `MonitorSocket` holds a raw `*mut udev_monitor` and isn't
    /// `Send`. We do a synchronous probe up front so init failures
    /// (missing libudev, kernel without netlink support) bubble up
    /// to the caller before we spin off a thread that just dies.
    pub fn new(
        vendor_id: u16,
        product_id: u16,
        event_tx: mpsc::UnboundedSender<HotplugEvent>,
    ) -> Result<Self, String> {
        // Probe-and-drop: catch init failures here.
        drop(build_monitor()?);

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);

        let thread_handle = thread::spawn(move || {
            let monitor = match build_monitor() {
                Ok(m) => m,
                Err(e) => {
                    error!(error = %e, "udev monitor rebuild in worker failed");
                    return;
                }
            };
            run_watcher(monitor, vendor_id, product_id, event_tx, stop_clone);
        });

        Ok(Self {
            stop,
            thread_handle: Some(thread_handle),
        })
    }

    /// Stop the hotplug watcher and join the worker.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.thread_handle.take() {
            // Worker wakes from poll(2) at most every 1s and notices
            // the stop flag — see `run_watcher`. Don't bother
            // shutting down the socket from here.
            let _ = handle.join();
        }
    }
}

impl Drop for HotplugWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

fn build_monitor() -> Result<udev::MonitorSocket, String> {
    udev::MonitorBuilder::new()
        .map_err(|e| format!("udev::MonitorBuilder::new: {e}"))?
        .match_subsystem_devtype("usb", "usb_device")
        .map_err(|e| format!("udev match_subsystem_devtype: {e}"))?
        .listen()
        .map_err(|e| format!("udev MonitorBuilder.listen: {e}"))
}

fn run_watcher(
    monitor: udev::MonitorSocket,
    vendor_id: u16,
    product_id: u16,
    event_tx: mpsc::UnboundedSender<HotplugEvent>,
    stop: Arc<AtomicBool>,
) {
    info!(
        "udev hotplug watcher started for VID:0x{:04X} PID:0x{:04X}",
        vendor_id, product_id
    );

    let fd = monitor.as_raw_fd();

    while !stop.load(Ordering::SeqCst) {
        // poll(2) with a 1s timeout so we re-check the stop flag
        // periodically. The pace is invisible to the user — actual
        // events still wake us instantly.
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pollfd, 1, 1000) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            error!(error = %err, "udev poll failed");
            // Tight-loop guard: brief sleep before retrying.
            thread::sleep(Duration::from_millis(200));
            continue;
        }
        if ret == 0 {
            // Timed out — back to the stop check.
            continue;
        }
        if pollfd.revents & libc::POLLIN == 0 {
            continue;
        }

        // Drain every event the kernel queued while we were away.
        // `MonitorSocket::iter()` returns a `SocketIter` that yields
        // every currently-readable event and then `None` (non-blocking
        // under the hood — it just calls `udev_monitor_receive_device`
        // until that returns NULL).
        for event in monitor.iter() {
            if !matches_device(&event, vendor_id, product_id) {
                continue;
            }
            match event.event_type() {
                udev::EventType::Add => {
                    info!("USB device arrived (udev)");
                    let _ = event_tx.send(HotplugEvent::DeviceArrived);
                }
                udev::EventType::Remove => {
                    info!("USB device removed (udev)");
                    let _ = event_tx.send(HotplugEvent::DeviceRemoved);
                }
                other => {
                    debug!(?other, "udev event ignored");
                }
            }
        }
    }

    info!("udev hotplug watcher stopped");
}

/// True when a `usb_device` event is for the right VID/PID. udev
/// exposes `idVendor` / `idProduct` as 4-char hex strings without a
/// `0x` prefix (e.g. `feed`, `0803`).
fn matches_device(event: &udev::Event, vendor_id: u16, product_id: u16) -> bool {
    let dev = event.device();
    let vid = dev
        .attribute_value("idVendor")
        .and_then(|s| s.to_str())
        .and_then(|s| u16::from_str_radix(s, 16).ok());
    let pid = dev
        .attribute_value("idProduct")
        .and_then(|s| s.to_str())
        .and_then(|s| u16::from_str_radix(s, 16).ok());
    match (vid, pid) {
        (Some(v), Some(p)) => v == vendor_id && p == product_id,
        _ => {
            // Some events (e.g. interface-level) won't have these
            // attributes. The `match_subsystem_devtype("usb",
            // "usb_device")` filter we set on the monitor should
            // already weed those out, but be defensive.
            warn!("udev event missing idVendor/idProduct");
            false
        }
    }
}

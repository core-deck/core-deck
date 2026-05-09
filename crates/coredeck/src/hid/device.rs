//! HID device discovery and connection management

use super::commands;
use super::protocol::{
    DeviceMode, DeviceState, HidCommand, HidPacket, ProtocolMode, ResponsePacket, SoftKeyConfig,
    SoftKeyType, PACKET_SIZE, VIAL_PREFIX,
};
use crate::state::{DaemonEvent, DaemonEventSender};
use crate::HidConfig;
use anyhow::{anyhow, Context, Result};
use hidapi::{HidApi, HidDevice};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

// Native USB hotplug — IOKit on macOS, libudev on Linux. Both
// backends export the same `HotplugEvent` enum + `HotplugWatcher`
// shape so the consumer below doesn't have to branch.
#[cfg(target_os = "linux")]
use super::hotplug_linux::{HotplugEvent, HotplugWatcher};
#[cfg(target_os = "macos")]
use super::hotplug_macos::{HotplugEvent, HotplugWatcher};

/// Number of consecutive ping failures before declaring disconnection
const DISCONNECT_THRESHOLD: u32 = 3;

/// Polling interval used by the enumeration fallback (when no native
/// hotplug backend is available, or when one fails to init). The
/// fallback is reachable on every platform.
const RECONNECT_INITIAL_MS: u64 = 500;
const RECONNECT_MAX_MS: u64 = 5000;

/// Manager for HID device communication with Core Deck
pub struct HidManager {
    /// HID API instance
    api: Arc<Mutex<HidApi>>,
    /// Connected device (if any)
    device: Arc<Mutex<Option<HidDevice>>>,
    /// Configuration
    config: HidConfig,
    /// Event sender for status updates (wakes event loop)
    event_tx: DaemonEventSender,
    /// Whether currently connected (HID interface open)
    connected: Arc<AtomicBool>,
    /// Whether the USB device is physically present (detected by enumeration/hotplug)
    device_available: Arc<AtomicBool>,
    /// Cached device name from enumeration (available without opening device)
    cached_device_name: Arc<Mutex<Option<String>>>,
    /// Whether the monitor thread should stop
    stop_monitor: Arc<AtomicBool>,
    /// Active protocol mode (Standalone=0, Vial=1), shared with all threads
    protocol_mode: Arc<AtomicU8>,
    /// Last display payload sent (for deduplication).
    /// Shared with monitor threads so disconnect clears it.
    last_display_payload: Arc<Mutex<String>>,
    /// Native USB hotplug watcher (macOS IOKit / Linux udev). None on
    /// platforms without a backend or when watcher init failed and the
    /// poll fallback took over.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    hotplug_watcher: Option<HotplugWatcher>,
}

impl HidManager {
    /// Create a new HID manager.
    ///
    /// Does NOT open the HID device — only enumerates to check availability.
    /// Call `open_device()` to actually open the device (when the app connects).
    pub fn new(config: HidConfig, event_tx: DaemonEventSender) -> Result<Self> {
        let api = HidApi::new().context("Failed to initialize HID API")?;

        // Don't seize the device exclusively on macOS — we only need the
        // vendor-specific raw-HID interface (0xFF60) and must not prevent
        // the system keyboard driver from receiving events on the standard
        // keyboard interface of the same composite USB device.
        #[cfg(target_os = "macos")]
        {
            api.set_open_exclusive(false);
        }

        // Check if device is physically present (enumerate only, no open)
        let (available, cached_name) = check_device_presence(&api, &config);
        if available {
            info!(
                "Device available: {}",
                cached_name.as_deref().unwrap_or("?")
            );
        } else {
            info!("Device not found during initial enumeration");
        }

        let mut manager = Self {
            api: Arc::new(Mutex::new(api)),
            device: Arc::new(Mutex::new(None)),
            config: config.clone(),
            event_tx: event_tx.clone(),
            connected: Arc::new(AtomicBool::new(false)),
            device_available: Arc::new(AtomicBool::new(available)),
            cached_device_name: Arc::new(Mutex::new(cached_name)),
            stop_monitor: Arc::new(AtomicBool::new(false)),
            protocol_mode: Arc::new(AtomicU8::new(ProtocolMode::Standalone as u8)),
            last_display_payload: Arc::new(Mutex::new(String::new())),
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            hotplug_watcher: None,
        };

        // Start the appropriate monitor mechanism (hotplug only tracks availability).
        // macOS uses IOKit, Linux uses udev; both expose the same shape and
        // the consumer below treats them identically. Anything else falls
        // back to enumeration polling.
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            manager.start_native_hotplug(config, event_tx);
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            manager.start_polling_monitor();
        }

        // Start ping thread for connection health monitoring
        manager.start_ping_thread();

        Ok(manager)
    }

    /// Start the native USB hotplug watcher. macOS uses IOKit
    /// matching notifications, Linux uses libudev's netlink monitor —
    /// both feed the same `HotplugEvent` channel and the consumer
    /// below doesn't care which.
    ///
    /// Only tracks device availability (plug/unplug). Does NOT open the device.
    /// If the device was open when removed, closes it and emits HidDisconnected.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn start_native_hotplug(&mut self, config: HidConfig, _event_tx: DaemonEventSender) {
        // Create channel for hotplug events
        let (hotplug_tx, mut hotplug_rx) = tokio::sync::mpsc::unbounded_channel();

        // Start the native watcher (IOKit / udev)
        match HotplugWatcher::new(config.vendor_id, config.product_id, hotplug_tx) {
            Ok(watcher) => {
                self.hotplug_watcher = Some(watcher);
                #[cfg(target_os = "macos")]
                info!("Started native IOKit hotplug watcher");
                #[cfg(target_os = "linux")]
                info!("Started native udev hotplug watcher");

                let api = Arc::clone(&self.api);
                let device = Arc::clone(&self.device);
                let connected = Arc::clone(&self.connected);
                let device_available = Arc::clone(&self.device_available);
                let cached_device_name = Arc::clone(&self.cached_device_name);
                let stop_monitor = Arc::clone(&self.stop_monitor);
                let protocol_mode = Arc::clone(&self.protocol_mode);
                let last_display_payload = Arc::clone(&self.last_display_payload);
                let event_tx = self.event_tx.clone();
                let config = self.config.clone();

                thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("Failed to create tokio runtime");

                    rt.block_on(async {
                        while !stop_monitor.load(Ordering::Relaxed) {
                            tokio::select! {
                                Some(event) = hotplug_rx.recv() => {
                                    match event {
                                        HotplugEvent::DeviceArrived => {
                                            // Small delay to let the device initialize
                                            tokio::time::sleep(Duration::from_millis(100)).await;

                                            // Refresh device list to see the new device
                                            {
                                                let mut api_guard = api.lock();
                                                let _ = api_guard.refresh_devices();
                                            }

                                            // Check presence (enumerate only, no open)
                                            let name = {
                                                let api_guard = api.lock();
                                                let (avail, name) = check_device_presence(&api_guard, &config);
                                                if avail {
                                                    device_available.store(true, Ordering::Relaxed);
                                                    *cached_device_name.lock() = name.clone();
                                                }
                                                name
                                            };

                                            let device_name = name.unwrap_or_else(|| "Core Deck".to_string());
                                            info!("Device available via hotplug: {}", device_name);
                                            let _ = event_tx.send(DaemonEvent::DeviceAvailable {
                                                device_name,
                                            });
                                        }
                                        HotplugEvent::DeviceRemoved => {
                                            device_available.store(false, Ordering::Relaxed);
                                            *cached_device_name.lock() = None;

                                            // If device was open, close it
                                            if connected.load(Ordering::Relaxed) {
                                                *device.lock() = None;
                                                connected.store(false, Ordering::Relaxed);
                                                protocol_mode.store(ProtocolMode::Standalone as u8, Ordering::Relaxed);
                                                *last_display_payload.lock() = String::new();
                                                let _ = event_tx.send(DaemonEvent::HidDisconnected);
                                                info!("Device disconnected via hotplug (was open)");
                                            }

                                            let _ = event_tx.send(DaemonEvent::DeviceUnavailable);
                                            info!("Device unavailable via hotplug");
                                        }
                                    }
                                }
                                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                                    if stop_monitor.load(Ordering::Relaxed) {
                                        break;
                                    }
                                }
                            }
                        }
                    });
                });
            }
            Err(e) => {
                warn!(
                    "Native hotplug watcher init failed: {}; falling back to enumeration polling",
                    e,
                );
                self.start_polling_monitor_internal();
            }
        }
    }

    /// Start polling-based monitor (for platforms without a native
    /// hotplug backend, or as a fallback when one fails to init).
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn start_polling_monitor(&self) {
        self.start_polling_monitor_internal();
    }

    /// Internal polling monitor implementation.
    ///
    /// Only tracks device availability (enumerate, no open). Does NOT open the device.
    fn start_polling_monitor_internal(&self) {
        let api = Arc::clone(&self.api);
        let device = Arc::clone(&self.device);
        let connected = Arc::clone(&self.connected);
        let device_available = Arc::clone(&self.device_available);
        let cached_device_name = Arc::clone(&self.cached_device_name);
        let stop_monitor = Arc::clone(&self.stop_monitor);
        let protocol_mode = Arc::clone(&self.protocol_mode);
        let last_display_payload = Arc::clone(&self.last_display_payload);
        let event_tx = self.event_tx.clone();
        let config = self.config.clone();

        thread::spawn(move || {
            info!("HID polling monitor thread started");

            let mut poll_interval_ms = RECONNECT_INITIAL_MS;
            let max_interval = RECONNECT_MAX_MS;

            let mut was_available = device_available.load(Ordering::Relaxed);

            while !stop_monitor.load(Ordering::Relaxed) {
                // Refresh device list
                {
                    let mut api_guard = api.lock();
                    if let Err(e) = api_guard.refresh_devices() {
                        debug!("Failed to refresh device list: {}", e);
                    }
                }

                // Check presence (enumerate only, no open)
                let (is_available, name) = {
                    let api_guard = api.lock();
                    check_device_presence(&api_guard, &config)
                };

                if is_available && !was_available {
                    // Device just appeared
                    device_available.store(true, Ordering::Relaxed);
                    *cached_device_name.lock() = name.clone();
                    let device_name = name.unwrap_or_else(|| "Core Deck".to_string());
                    info!("Device available via polling: {}", device_name);
                    let _ = event_tx.send(DaemonEvent::DeviceAvailable { device_name });
                    poll_interval_ms = 500;
                } else if !is_available && was_available {
                    // Device just disappeared
                    device_available.store(false, Ordering::Relaxed);
                    *cached_device_name.lock() = None;

                    // If device was open, close it
                    if connected.load(Ordering::Relaxed) {
                        *device.lock() = None;
                        connected.store(false, Ordering::Relaxed);
                        protocol_mode.store(ProtocolMode::Standalone as u8, Ordering::Relaxed);
                        *last_display_payload.lock() = String::new();
                        let _ = event_tx.send(DaemonEvent::HidDisconnected);
                        info!("Device disconnected via polling (was open)");
                    }

                    let _ = event_tx.send(DaemonEvent::DeviceUnavailable);
                    info!("Device unavailable via polling");
                } else if !is_available {
                    poll_interval_ms = (poll_interval_ms * 3 / 2).min(max_interval);
                }

                was_available = is_available;
                thread::sleep(Duration::from_millis(poll_interval_ms));
            }
            info!("HID polling monitor thread stopped");
        });
    }

    /// Start reader thread for connection health monitoring and incoming key events.
    ///
    /// This thread performs two duties:
    /// 1. Sends ping keepalives on a timer to detect disconnection
    /// 2. Polls for incoming device-initiated packets (key events, type strings, state reports)
    fn start_ping_thread(&self) {
        let device = Arc::clone(&self.device);
        let connected = Arc::clone(&self.connected);
        let stop_monitor = Arc::clone(&self.stop_monitor);
        let protocol_mode = Arc::clone(&self.protocol_mode);
        let last_display_payload = Arc::clone(&self.last_display_payload);
        let event_tx = self.event_tx.clone();
        let ping_interval = Duration::from_millis(self.config.ping_interval_ms);

        thread::spawn(move || {
            info!("HID reader thread started");
            let mut consecutive_failures: u32 = 0;
            let mut last_ping = Instant::now() - ping_interval; // trigger immediate first ping
            let mut type_string_buf: Vec<u8> = Vec::new();

            while !stop_monitor.load(Ordering::Relaxed) {
                if !connected.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }

                // --- Ping on timer ---
                if last_ping.elapsed() >= ping_interval {
                    let mode = ProtocolMode::from_byte(protocol_mode.load(Ordering::Relaxed));
                    let ping_ok = {
                        let device_guard = device.lock();
                        if let Some(ref dev) = *device_guard {
                            let packets = commands::build_ping(mode);
                            match send_packets_to_device(dev, &packets, mode) {
                                Ok(()) => {
                                    debug!("Ping sent");
                                    // Read pong response
                                    match read_raw_packet(dev, 100, mode) {
                                        Ok(Some(pkt)) => {
                                            dispatch_incoming_packet(
                                                &pkt,
                                                &event_tx,
                                                &mut type_string_buf,
                                            );
                                            true
                                        }
                                        Ok(None) => {
                                            debug!("No pong response");
                                            true // Write succeeded, device might be busy
                                        }
                                        Err(e) => {
                                            warn!("Error reading pong: {}", e);
                                            false
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!("Failed to send ping: {}", e);
                                    false
                                }
                            }
                        } else {
                            false
                        }
                    };

                    last_ping = Instant::now();

                    if ping_ok {
                        consecutive_failures = 0;
                    } else {
                        consecutive_failures += 1;
                        warn!(
                            "Ping failure {} of {}",
                            consecutive_failures, DISCONNECT_THRESHOLD
                        );

                        if consecutive_failures >= DISCONNECT_THRESHOLD {
                            info!("Device disconnected (consecutive ping failures)");
                            *device.lock() = None;
                            connected.store(false, Ordering::Relaxed);
                            protocol_mode.store(ProtocolMode::Standalone as u8, Ordering::Relaxed);
                            *last_display_payload.lock() = String::new();
                            let _ = event_tx.send(DaemonEvent::HidDisconnected);
                            consecutive_failures = 0;
                            type_string_buf.clear();
                            continue;
                        }
                    }
                }

                // --- Poll for incoming device-initiated packets ---
                // Use try_lock to avoid blocking command sends (send_display_update, etc.)
                if let Some(device_guard) = device.try_lock() {
                    if let Some(ref dev) = *device_guard {
                        let poll_mode =
                            ProtocolMode::from_byte(protocol_mode.load(Ordering::Relaxed));
                        match read_raw_packet(dev, 20, poll_mode) {
                            Ok(Some(pkt)) => {
                                dispatch_incoming_packet(&pkt, &event_tx, &mut type_string_buf);
                            }
                            Ok(None) => {} // Timeout, no data
                            Err(e) => {
                                debug!("Poll read error: {}", e);
                            }
                        }
                    }
                }
                // Brief yield if nothing happened to avoid busy-wait
                // (the 20ms read timeout above provides the main throttle)
            }
            info!("HID reader thread stopped");
        });
    }

    /// Try to connect to the Core Deck device
    pub fn try_connect(&self) -> Result<()> {
        let api = self.api.lock();

        // Find device by VID/PID and usage page
        let device_info = api
            .device_list()
            .find(|d| {
                d.vendor_id() == self.config.vendor_id
                    && d.product_id() == self.config.product_id
                    && d.usage_page() == self.config.usage_page
                    && d.usage() == self.config.usage_id
            })
            .ok_or_else(|| {
                anyhow!(
                    "Core Deck not found (VID: 0x{:04X}, PID: 0x{:04X}, Usage: 0x{:04X}/0x{:02X})",
                    self.config.vendor_id,
                    self.config.product_id,
                    self.config.usage_page,
                    self.config.usage_id
                )
            })?;

        let device_name = format!(
            "{} {}",
            device_info.manufacturer_string().unwrap_or("").trim(),
            device_info.product_string().unwrap_or("Core Deck").trim()
        )
        .trim()
        .to_string();

        info!("Found Core Deck: {}", device_name);

        // Open the device
        let device = device_info
            .open_device(&api)
            .context("Failed to open HID device")?;

        // Set non-blocking mode
        device
            .set_blocking_mode(false)
            .context("Failed to set non-blocking mode")?;

        // Detect protocol mode and firmware version before storing
        let (detected_mode, firmware_version) = detect_protocol_mode(&device, &self.event_tx);
        self.protocol_mode
            .store(detected_mode as u8, Ordering::Relaxed);

        // Store device
        *self.device.lock() = Some(device);
        self.connected.store(true, Ordering::Relaxed);

        // Notify connection
        let _ = self.event_tx.send(DaemonEvent::HidConnected {
            device_name,
            firmware_version,
        });

        info!("Connected to Core Deck");
        Ok(())
    }

    /// Check if device is connected (HID interface open)
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Check if device is physically present (USB enumerated, not necessarily open)
    pub fn is_device_available(&self) -> bool {
        self.device_available.load(Ordering::Relaxed)
    }

    /// Get cached device name from enumeration (available without opening)
    pub fn cached_device_name(&self) -> Option<String> {
        self.cached_device_name.lock().clone()
    }

    /// Open the HID device for communication.
    ///
    /// Called by WS handler when the app connects. On macOS this causes
    /// IOHIDDeviceOpen which captures key-up events from the keyboard
    /// interface — this is intentional while the app is connected.
    pub fn open_device(&self) -> Result<()> {
        if self.connected.load(Ordering::Relaxed) {
            return Ok(()); // Already open
        }
        self.try_connect()
    }

    /// Close the HID device, releasing the IOKit HID handle.
    ///
    /// Sends a Disconnect command first so the firmware immediately goes
    /// idle (restores key routing to system keyboard, shows logo, dims).
    /// Called by WS handler when the app disconnects and by HTTP handlers
    /// after transient operations.
    pub fn close_device(&self) {
        let mut device_guard = self.device.lock();
        if let Some(ref dev) = *device_guard {
            // Tell firmware we're going away — it will immediately go idle
            let mode = self.mode();
            let packets = commands::build_disconnect(mode);
            if let Err(e) = send_packets_to_device(dev, &packets, mode) {
                debug!("Failed to send disconnect: {}", e);
            }
        }
        if device_guard.take().is_some() {
            self.connected.store(false, Ordering::Relaxed);
            self.protocol_mode
                .store(ProtocolMode::Standalone as u8, Ordering::Relaxed);
            *self.last_display_payload.lock() = String::new();
            // Don't emit HidDisconnected — the WS handler manages the lifecycle
            info!("HID device closed (released to system)");
        }
    }

    /// Current protocol mode (Standalone or Vial)
    fn mode(&self) -> ProtocolMode {
        ProtocolMode::from_byte(self.protocol_mode.load(Ordering::Relaxed))
    }

    /// Send a display update. Skips sending if the payload is identical to the last one sent.
    pub fn send_display_update(&self, update: &coredeck_protocol::DisplayUpdate) -> Result<()> {
        // Build a dedup key from serialized JSON
        let payload_key = serde_json::to_string(update).unwrap_or_default();
        {
            let mut last = self.last_display_payload.lock();
            if *last == payload_key {
                return Ok(());
            }
            *last = payload_key;
        }

        let device_guard = self.device.lock();
        let device = device_guard
            .as_ref()
            .ok_or_else(|| anyhow!("Device not connected"))?;

        let mode = self.mode();
        let packets = commands::build_display_update(update, mode);
        send_packets_to_device(device, &packets, mode)?;

        self.drain_response(device);

        Ok(())
    }

    /// Query firmware version from the device.
    /// Returns the version string, or a fallback if the device doesn't support the command.
    pub fn query_version(&self) -> String {
        let device_guard = self.device.lock();
        match device_guard.as_ref() {
            Some(device) => {
                let mode = self.mode();
                let packets = commands::build_get_version(mode);
                if let Err(e) = send_packets_to_device(device, &packets, mode) {
                    debug!("Failed to send GetVersion: {}", e);
                    return "unknown".to_string();
                }
                match read_response(device, HidCommand::GetVersion, &self.event_tx, mode) {
                    Ok(response) if response.status == 0 => {
                        let version = String::from_utf8_lossy(&response.data).trim().to_string();
                        if version.is_empty() {
                            "unknown".to_string()
                        } else {
                            version
                        }
                    }
                    _ => "unknown".to_string(),
                }
            }
            None => "unknown".to_string(),
        }
    }

    /// Set display brightness (chunked protocol)
    pub fn set_brightness(&self, level: u8, save: bool) -> Result<()> {
        let device_guard = self.device.lock();
        let device = device_guard
            .as_ref()
            .ok_or_else(|| anyhow!("Device not connected"))?;

        let mode = self.mode();
        let packets = commands::build_set_brightness(level, save, mode);
        send_packets_to_device(device, &packets, mode)?;

        // Read response
        self.drain_response(device);

        info!("Brightness set to {}", level);
        Ok(())
    }

    /// Set a soft key assignment.
    ///
    /// When `save=true` the firmware blocks USB processing on an EEPROM
    /// flash write (100-500ms on RP2040) before sending its reply, so we
    /// widen the first-packet timeout for that case. The default 200ms
    /// surfaces flash-stalls as spurious "Timeout waiting for response"
    /// errors and leaves a stale set-reply in the read buffer that
    /// confuses the next `get_soft_key`.
    pub fn set_soft_key(
        &self,
        index: u8,
        key_type: SoftKeyType,
        data: &[u8],
        save: bool,
    ) -> Result<()> {
        let device_guard = self.device.lock();
        let device = device_guard
            .as_ref()
            .ok_or_else(|| anyhow!("Device not connected"))?;

        let mode = self.mode();
        let packets = commands::build_set_soft_key(index, key_type, data, save, mode);
        send_packets_to_device(device, &packets, mode)?;

        let first_timeout_ms = if save { 1500 } else { 200 };
        let _ = read_response_with_timeout(
            device,
            HidCommand::SetSoftKey,
            &self.event_tx,
            mode,
            first_timeout_ms,
        )?;

        info!("Soft key {} set", index);
        Ok(())
    }

    /// Get a soft key configuration.
    ///
    /// Uses a widened first-packet timeout because the settings page's
    /// `refreshSoftkeys` is often called immediately after a `save=true`
    /// `set_soft_key`. The firmware finishes its EEPROM write and emits
    /// a state report, and processing the next inbound command can drift
    /// past the default 200ms — long enough to surface as a spurious
    /// "Timeout waiting for response" partway through the three-key
    /// refresh.
    pub fn get_soft_key(&self, index: u8) -> Result<SoftKeyConfig> {
        let device_guard = self.device.lock();
        let device = device_guard
            .as_ref()
            .ok_or_else(|| anyhow!("Device not connected"))?;

        let mode = self.mode();
        let packets = commands::build_get_soft_key(index, mode);
        send_packets_to_device(device, &packets, mode)?;

        // Read response — expect chunked response with key config data
        let response =
            read_response_with_timeout(device, HidCommand::GetSoftKey, &self.event_tx, mode, 1000)?;

        // Parse response: [key_index, key_type, ...entry_data]
        // The firmware sends: send_response(cmd, status=0x00, [key_index, type, data...])
        // read_response() strips the status byte, so response.data = [key_index, type, entry_data...]
        if response.data.len() < 2 {
            return Ok(SoftKeyConfig {
                index,
                key_type: SoftKeyType::Default,
                data: vec![],
            });
        }

        let _key_index = response.data[0];
        let raw_type = SoftKeyType::from_byte(response.data[1]).unwrap_or(SoftKeyType::Default);
        let data = if response.data.len() > 2 {
            response.data[2..].to_vec()
        } else {
            vec![]
        };

        // Older firmware reports `Default` (0) directly with the resolved
        // keymap keycode appended (`[type=0, kc_hi, kc_lo]`); newer firmware
        // substitutes the type byte to `Keycode` (1). Either way the data
        // bytes are already a 16-bit keycode for the user-visible "what
        // does this key do" question — normalise to Keycode here so the
        // settings page never has to special-case Default.
        let key_type = if raw_type == SoftKeyType::Default && data.len() == 2 {
            SoftKeyType::Keycode
        } else {
            raw_type
        };

        Ok(SoftKeyConfig {
            index,
            key_type,
            data,
        })
    }

    /// Reset all soft keys to defaults
    ///
    /// Returns the effective assignment for each key post-reset.
    /// Format from firmware: [type, kc_hi, kc_lo] x 3
    pub fn reset_soft_keys(&self) -> Result<[SoftKeyConfig; 3]> {
        let device_guard = self.device.lock();
        let device = device_guard
            .as_ref()
            .ok_or_else(|| anyhow!("Device not connected"))?;

        let mode = self.mode();
        let packets = commands::build_reset_soft_keys(mode);
        send_packets_to_device(device, &packets, mode)?;

        // Read the response — firmware now returns effective assignments
        let response = read_response(device, HidCommand::ResetSoftKeys, &self.event_tx, mode)?;

        // Parse response data: [type, kc_hi, kc_lo] x 3
        let mut configs = [
            SoftKeyConfig {
                index: 0,
                key_type: SoftKeyType::Default,
                data: vec![],
            },
            SoftKeyConfig {
                index: 1,
                key_type: SoftKeyType::Default,
                data: vec![],
            },
            SoftKeyConfig {
                index: 2,
                key_type: SoftKeyType::Default,
                data: vec![],
            },
        ];

        for i in 0..3usize {
            let offset = i * 3;
            if offset + 2 < response.data.len() {
                let key_type =
                    SoftKeyType::from_byte(response.data[offset]).unwrap_or(SoftKeyType::Default);
                let kc_hi = response.data[offset + 1];
                let kc_lo = response.data[offset + 2];
                configs[i] = SoftKeyConfig {
                    index: i as u8,
                    key_type,
                    data: match key_type {
                        SoftKeyType::Keycode | SoftKeyType::Default => vec![kc_hi, kc_lo],
                        // String/Sequence only have kc=0 in the 0x06 response
                        _ => vec![],
                    },
                };
            }
        }

        info!("Soft keys reset to defaults");
        Ok(configs)
    }

    /// Send an alert overlay to the device
    pub fn send_alert(
        &self,
        tab: usize,
        session: &str,
        text: &str,
        details: Option<&str>,
    ) -> Result<()> {
        let device_guard = self.device.lock();
        let device = device_guard
            .as_ref()
            .ok_or_else(|| anyhow!("Device not connected"))?;

        let mode = self.mode();
        let packets = commands::build_alert(tab, session, text, details, mode);
        send_packets_to_device(device, &packets, mode)?;

        self.drain_response(device);

        info!("Alert sent: tab={}, text={}", tab, text);
        Ok(())
    }

    /// Clear an alert overlay on the device
    pub fn clear_alert(&self, tab: usize) -> Result<()> {
        let device_guard = self.device.lock();
        let device = device_guard
            .as_ref()
            .ok_or_else(|| anyhow!("Device not connected"))?;

        let mode = self.mode();
        let packets = commands::build_clear_alert(tab, mode);
        send_packets_to_device(device, &packets, mode)?;

        self.drain_response(device);

        debug!("Alert cleared: tab={}", tab);
        Ok(())
    }

    /// Set device LED mode
    pub fn set_mode(&self, mode: DeviceMode) -> Result<()> {
        let device_guard = self.device.lock();
        let device = device_guard
            .as_ref()
            .ok_or_else(|| anyhow!("Device not connected"))?;

        let proto_mode = self.mode();
        let packets = commands::build_set_mode(mode, proto_mode);
        send_packets_to_device(device, &packets, proto_mode)?;

        self.drain_response(device);

        debug!("Device mode set to {}", mode);
        Ok(())
    }

    /// Read and discard response packets, forwarding key/string events but NOT state reports.
    /// State reports from command confirmations are consumed silently — the reader thread
    /// handles device-initiated state reports (button presses, YOLO switch).
    fn drain_response(&self, device: &HidDevice) {
        let mode = self.mode();
        let mut type_string_buf = Vec::new();
        for _ in 0..3 {
            match read_raw_packet(device, 50, mode) {
                Ok(Some(pkt)) => {
                    let is_device_initiated = matches!(
                        pkt.command(),
                        Some(HidCommand::StateReport)
                            | Some(HidCommand::KeyEvent)
                            | Some(HidCommand::TypeString)
                            | Some(HidCommand::Ping)
                    );
                    // Forward key/string/ping events but skip StateReport —
                    // it's a confirmation echo, not a user action
                    if is_device_initiated && pkt.command() != Some(HidCommand::StateReport) {
                        dispatch_incoming_packet(&pkt, &self.event_tx, &mut type_string_buf);
                    }
                    // If this is END packet of a response, we're done
                    if pkt.is_end() && !is_device_initiated {
                        break;
                    }
                }
                Ok(None) => break, // Timeout, no more data
                Err(_) => break,
            }
        }
    }

    /// Disconnect from the device
    pub fn disconnect(&self) {
        let mut device_guard = self.device.lock();
        if device_guard.take().is_some() {
            self.connected.store(false, Ordering::Relaxed);
            self.protocol_mode
                .store(ProtocolMode::Standalone as u8, Ordering::Relaxed);
            // Clear dedup cache so the next connect sends a fresh display update
            *self.last_display_payload.lock() = String::new();
            let _ = self.event_tx.send(DaemonEvent::HidDisconnected);
            info!("Disconnected from Core Deck");
        }
    }
}

impl Drop for HidManager {
    fn drop(&mut self) {
        self.stop_monitor.store(true, Ordering::Relaxed);
        #[cfg(target_os = "macos")]
        {
            if let Some(ref mut watcher) = self.hotplug_watcher {
                watcher.stop();
            }
        }
        self.disconnect();
    }
}

/// Check if the device is physically present by enumerating (no device open).
/// Returns `(available, device_name)`.
fn check_device_presence(api: &HidApi, config: &HidConfig) -> (bool, Option<String>) {
    match api.device_list().find(|d| {
        d.vendor_id() == config.vendor_id
            && d.product_id() == config.product_id
            && d.usage_page() == config.usage_page
            && d.usage() == config.usage_id
    }) {
        Some(info) => {
            let name = format!(
                "{} {}",
                info.manufacturer_string().unwrap_or("").trim(),
                info.product_string().unwrap_or("Core Deck").trim()
            )
            .trim()
            .to_string();
            (true, Some(name))
        }
        None => (false, None),
    }
}

/// Detect protocol mode and firmware version from an already-opened device.
///
/// Tries VIAL-prefixed GetVersion first. If the response starts with `0x80` and
/// parses as a valid version, the device uses VIAL mode. Otherwise falls back to
/// standalone GetVersion.
fn detect_protocol_mode(
    device: &HidDevice,
    event_tx: &DaemonEventSender,
) -> (ProtocolMode, String) {
    // --- Phase 1: try VIAL mode ---
    let vial_packets = commands::build_get_version(ProtocolMode::Vial);
    if send_packets_to_device(device, &vial_packets, ProtocolMode::Vial).is_ok() {
        match read_response(device, HidCommand::GetVersion, event_tx, ProtocolMode::Vial) {
            Ok(response) if response.status == 0 => {
                let version = String::from_utf8_lossy(&response.data).trim().to_string();
                if !version.is_empty() {
                    info!("Detected VIAL protocol mode, firmware {}", version);
                    return (ProtocolMode::Vial, version);
                }
            }
            Ok(_) => {}
            Err(e) => {
                debug!("VIAL GetVersion failed (expected for standalone): {}", e);
            }
        }
    }

    // --- Phase 2: drain leftover, try standalone ---
    // Drain any leftover responses from the VIAL probe
    for _ in 0..5 {
        match read_raw_packet(device, 50, ProtocolMode::Standalone) {
            Ok(Some(_)) => continue,
            _ => break,
        }
    }

    let standalone_packets = commands::build_get_version(ProtocolMode::Standalone);
    if let Err(e) = send_packets_to_device(device, &standalone_packets, ProtocolMode::Standalone) {
        debug!("Failed to send standalone GetVersion: {}", e);
        return (ProtocolMode::Standalone, "unknown".to_string());
    }

    match read_response(
        device,
        HidCommand::GetVersion,
        event_tx,
        ProtocolMode::Standalone,
    ) {
        Ok(response) if response.status == 0 => {
            let version = String::from_utf8_lossy(&response.data).trim().to_string();
            let version = if version.is_empty() {
                "unknown".to_string()
            } else {
                version
            };
            info!("Detected standalone protocol mode, firmware {}", version);
            (ProtocolMode::Standalone, version)
        }
        Ok(response) => {
            debug!("GetVersion returned status 0x{:02X}", response.status);
            (ProtocolMode::Standalone, "unknown".to_string())
        }
        Err(e) => {
            debug!("GetVersion not supported or failed: {}", e);
            (ProtocolMode::Standalone, "unknown".to_string())
        }
    }
}

/// Send multiple packets (chunks) to the HID device sequentially
fn send_packets_to_device(
    device: &HidDevice,
    packets: &[HidPacket],
    mode: ProtocolMode,
) -> Result<()> {
    for packet in packets {
        send_single_packet(device, packet, mode)?;
    }
    Ok(())
}

/// Send a single 32-byte packet to the HID device.
///
/// In VIAL mode the wire bytes become `[0x80, flags, cmd, payload×29]` — the VIAL prefix
/// is prepended and the last payload byte is dropped to stay within 32 bytes.
fn send_single_packet(device: &HidDevice, packet: &HidPacket, mode: ProtocolMode) -> Result<()> {
    let bytes = packet.as_bytes();

    let wire: [u8; PACKET_SIZE] = match mode {
        ProtocolMode::Vial => {
            let mut buf = [0u8; PACKET_SIZE];
            buf[0] = VIAL_PREFIX;
            buf[1..PACKET_SIZE].copy_from_slice(&bytes[..PACKET_SIZE - 1]);
            buf
        }
        ProtocolMode::Standalone => *bytes,
    };

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let data = {
        let mut data = Vec::with_capacity(PACKET_SIZE + 1);
        data.push(0x00); // Report ID
        data.extend_from_slice(&wire);
        data
    };

    #[cfg(target_os = "linux")]
    let data = wire.to_vec();

    let written = device
        .write(&data)
        .context("Failed to write to HID device")?;

    debug!("Wrote {} bytes to HID device (mode={:?})", written, mode);

    Ok(())
}

/// Read a single raw HID packet with timeout.
///
/// In VIAL mode: if `buffer[0] != 0x80` the packet is a VIA echo and is discarded
/// (returns `Ok(None)`). Otherwise the prefix is stripped by shifting bytes left by 1.
fn read_raw_packet(
    device: &HidDevice,
    timeout_ms: i32,
    mode: ProtocolMode,
) -> Result<Option<HidPacket>> {
    let mut buffer = [0u8; PACKET_SIZE];
    match device.read_timeout(&mut buffer, timeout_ms) {
        Ok(n) if n > 0 => {
            if mode == ProtocolMode::Vial {
                if buffer[0] != VIAL_PREFIX {
                    // Not a VIAL-prefixed response — discard (VIA echo)
                    debug!("Discarding non-VIAL packet (byte0=0x{:02X})", buffer[0]);
                    return Ok(None);
                }
                // Strip prefix: shift left by 1
                let mut stripped = [0u8; PACKET_SIZE];
                stripped[..PACKET_SIZE - 1].copy_from_slice(&buffer[1..PACKET_SIZE]);
                Ok(Some(HidPacket::from_bytes(&stripped)))
            } else {
                Ok(Some(HidPacket::from_bytes(&buffer)))
            }
        }
        Ok(_) => Ok(None), // Timeout
        Err(e) => Err(anyhow!("HID read error: {}", e)),
    }
}

/// Read a complete chunked response for a specific command.
/// Transparently handles interleaved state reports by dispatching them as events.
fn read_response(
    device: &HidDevice,
    expected_cmd: HidCommand,
    event_tx: &DaemonEventSender,
    mode: ProtocolMode,
) -> Result<ResponsePacket> {
    read_response_with_timeout(device, expected_cmd, event_tx, mode, 200)
}

/// Like `read_response` but lets the caller widen the wait for the
/// *first* packet. Used by `set_soft_key(save=true)` because the
/// firmware blocks USB processing during EEPROM flash writes
/// (100-500ms) before sending its reply, and the default 200ms first
/// timeout would surface that as a spurious "timeout" error.
fn read_response_with_timeout(
    device: &HidDevice,
    expected_cmd: HidCommand,
    event_tx: &DaemonEventSender,
    mode: ProtocolMode,
    first_timeout_ms: i32,
) -> Result<ResponsePacket> {
    let mut payload = Vec::new();
    let mut got_start = false;
    let mut _command_byte = 0u8;
    let mut type_string_buf = Vec::new();

    // Read packets until we get a complete response (up to reasonable limit)
    for _ in 0..20 {
        let timeout = if got_start { 200 } else { first_timeout_ms };
        let pkt = match read_raw_packet(device, timeout, mode)? {
            Some(pkt) => pkt,
            None => {
                if got_start {
                    // Timeout mid-response
                    return Err(anyhow!("Timeout waiting for response continuation"));
                } else {
                    return Err(anyhow!("Timeout waiting for response"));
                }
            }
        };

        // Forward device-initiated packets (state reports, key events, etc.)
        let is_device_initiated = matches!(
            pkt.command(),
            Some(HidCommand::StateReport)
                | Some(HidCommand::KeyEvent)
                | Some(HidCommand::TypeString)
                | Some(HidCommand::Ping)
        );
        if is_device_initiated {
            dispatch_incoming_packet(&pkt, event_tx, &mut type_string_buf);
            continue;
        }

        // Check command matches
        if pkt.command() != Some(expected_cmd) && pkt.command() != Some(HidCommand::Error) {
            debug!(
                "Unexpected response command: {:?} (expected {:?})",
                pkt.command(),
                expected_cmd
            );
            continue;
        }

        if pkt.is_start() {
            got_start = true;
            _command_byte = pkt.command_byte();
            payload.clear();
        }

        if got_start {
            payload.extend_from_slice(pkt.payload());
        }

        if pkt.is_end() && got_start {
            // Complete response assembled
            // Trim trailing zeros from the last chunk
            while payload.last() == Some(&0) {
                payload.pop();
            }

            let status = if payload.is_empty() { 0 } else { payload[0] };
            let data = if payload.len() > 1 {
                payload[1..].to_vec()
            } else {
                vec![]
            };

            return Ok(ResponsePacket { status, data });
        }
    }

    Err(anyhow!("Response read exceeded maximum packet count"))
}

/// Dispatch a single incoming packet from the device, emitting appropriate AppEvents.
///
/// Handles: StateReport, KeyEvent, TypeString, Ping (pong). All other commands are ignored
/// (they are responses to host-initiated commands handled elsewhere).
///
/// `type_string_buf` accumulates chunked TypeString payloads across calls.
fn dispatch_incoming_packet(
    pkt: &HidPacket,
    event_tx: &DaemonEventSender,
    type_string_buf: &mut Vec<u8>,
) {
    match pkt.command() {
        Some(HidCommand::StateReport) => {
            let state_byte = pkt.payload()[0];
            let ds = DeviceState::from_byte(state_byte);
            debug!("State report: mode={}, yolo={}", ds.mode, ds.yolo);
            let _ = event_tx.send(DaemonEvent::DeviceStateChanged {
                mode: ds.mode,
                yolo: ds.yolo,
            });
        }
        Some(HidCommand::KeyEvent) => {
            // Payload: [keycode_hi, keycode_lo]
            let payload = pkt.payload();
            if payload.len() >= 2 {
                let keycode = ((payload[0] as u16) << 8) | (payload[1] as u16);
                debug!("Key event: keycode=0x{:04X}", keycode);
                let _ = event_tx.send(DaemonEvent::HidKeyEvent { keycode });
            }
        }
        Some(HidCommand::TypeString) => {
            // Chunked: accumulate payload, dispatch on END packet
            // Payload format: [flags_byte, ...string_data]
            // flags_byte bit 0: send_enter
            let payload = pkt.payload();

            if pkt.is_start() {
                type_string_buf.clear();
            }

            // Append raw payload (first byte of first chunk has flags)
            type_string_buf.extend_from_slice(payload);

            if pkt.is_end() && !type_string_buf.is_empty() {
                // First byte is flags, rest is UTF-8 string
                let flags = type_string_buf[0];
                let send_enter = flags & 0x01 != 0;

                // Trim trailing zeros from the string portion
                let mut str_bytes = &type_string_buf[1..];
                while str_bytes.last() == Some(&0) {
                    str_bytes = &str_bytes[..str_bytes.len() - 1];
                }

                if let Ok(text) = std::str::from_utf8(str_bytes) {
                    debug!("Type string: {:?} (send_enter={})", text, send_enter);
                    let _ = event_tx.send(DaemonEvent::HidTypeString {
                        text: text.to_string(),
                        send_enter,
                    });
                } else {
                    warn!("TypeString payload is not valid UTF-8");
                }
                type_string_buf.clear();
            }
        }
        Some(HidCommand::Ping) => {
            debug!("Pong received");
        }
        _ => {
            // Command response or unknown — ignore in the reader loop
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hid_config_default() {
        let config = HidConfig::default();
        assert_eq!(config.vendor_id, 0xFEED);
        assert_eq!(config.product_id, 0x0803);
        assert_eq!(config.usage_page, 0xFF60);
        assert_eq!(config.usage_id, 0x61);
    }
}

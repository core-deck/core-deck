//! Daemon tray icon and menu
//!
//! Simplified tray for the daemon: device status, Show/Hide app, Quit.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use coredeck_protocol::{WrapperTab, WrapperTabList};
use tray_icon::{
    menu::{IsMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
    TrayIcon as TrayIconHandle, TrayIconBuilder,
};
use tracing::{debug, error, info};

/// Device presence for tray display
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevicePresence {
    /// No device plugged in
    None,
    /// Device plugged in but HID interface not open
    Available,
    /// Device plugged in AND HID interface open (app connected)
    Active,
}

/// Tray menu actions
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonTrayAction {
    /// Focus a specific wrapper (the user clicked its tab in the menu).
    FocusWrapper(String),
    /// Open the settings page in the user's default browser.
    OpenSettings,
    /// Quit the daemon
    Quit,
}

/// Daemon tray manager
pub struct DaemonTrayManager {
    tray: TrayIconHandle,
    icons: TrayIcons,
    menu: Menu,
    /// Disabled top-of-menu items showing the connected device.
    device_name_item: MenuItem,
    device_firmware_item: MenuItem,
    /// Dynamic per-wrapper tab entries — replaced on every `set_tabs` call.
    /// Kept owned here so their menu IDs stay valid for click dispatch.
    tab_items: Vec<MenuItem>,
    /// Disabled placeholder shown when no wrappers are connected.
    empty_placeholder: Option<MenuItem>,
    /// MenuId → wrapper_id, used by the event thread to translate clicks
    /// on dynamic tab entries into `FocusWrapper` actions.
    tab_dispatch: Arc<Mutex<HashMap<MenuId, String>>>,
}

impl DaemonTrayManager {
    pub fn new() -> Result<(Self, std::sync::mpsc::Receiver<DaemonTrayAction>)> {
        let icons = TrayIcons::new().context("Failed to load tray icons")?;

        let menu = Menu::new();

        let device_name_item = MenuItem::new("No device", false, None);
        let device_firmware_item = MenuItem::new("Firmware —", false, None);

        let settings_item = MenuItem::new("Open Settings…", true, None);
        let settings_id = settings_item.id().clone();

        let quit_item = MenuItem::new("Quit Daemon", true, None);
        let quit_id = quit_item.id().clone();

        // Initial layout: [device name, firmware, separator, empty placeholder,
        // separator, Settings, Quit]. Tab entries replace the placeholder
        // on the first `set_tabs` call (DYNAMIC_OFFSET = 3, so Settings/Quit
        // stay after the second separator regardless of how many tabs).
        let empty = MenuItem::new("No Claude sessions", false, None);
        menu.append(&device_name_item)?;
        menu.append(&device_firmware_item)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&empty)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&settings_item)?;
        menu.append(&quit_item)?;

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu.clone()))
            .with_tooltip("Core Deck Daemon - Disconnected")
            .with_icon(icons.disconnected().clone())
            .build()
            .context("Failed to create tray icon")?;

        info!("Daemon tray icon created");

        let (action_tx, action_rx) = std::sync::mpsc::channel();
        let tab_dispatch: Arc<Mutex<HashMap<MenuId, String>>> = Arc::new(Mutex::new(HashMap::new()));

        // Menu event handler thread
        let quit_id_clone = quit_id.clone();
        let settings_id_clone = settings_id.clone();
        let tab_dispatch_clone = Arc::clone(&tab_dispatch);
        std::thread::spawn(move || {
            let receiver = MenuEvent::receiver();
            loop {
                if let Ok(event) = receiver.recv() {
                    debug!("Daemon menu event: {:?}", event);
                    let action = if event.id == quit_id_clone {
                        Some(DaemonTrayAction::Quit)
                    } else if event.id == settings_id_clone {
                        Some(DaemonTrayAction::OpenSettings)
                    } else {
                        tab_dispatch_clone
                            .lock()
                            .ok()
                            .and_then(|m| m.get(&event.id).cloned())
                            .map(DaemonTrayAction::FocusWrapper)
                    };
                    if let Some(action) = action {
                        if action_tx.send(action).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let manager = Self {
            tray,
            icons,
            menu,
            device_name_item,
            device_firmware_item,
            tab_items: Vec::new(),
            empty_placeholder: Some(empty),
            tab_dispatch,
        };

        Ok((manager, action_rx))
    }

    /// Replace the dynamic tab section with entries from `list`. The
    /// active wrapper is marked with a leading "● ". When the list is
    /// empty, restore a disabled "No Claude sessions" placeholder.
    pub fn set_tabs(&mut self, list: &WrapperTabList) {
        // Insert below the device-info section: [name, firmware, separator, ...].
        const DYNAMIC_OFFSET: usize = 3;

        // Remove existing dynamic items (placeholder or previous tabs).
        if let Some(item) = self.empty_placeholder.take() {
            let _ = self.menu.remove(&item as &dyn IsMenuItem);
        }
        for item in self.tab_items.drain(..) {
            let _ = self.menu.remove(&item as &dyn IsMenuItem);
        }
        if let Ok(mut map) = self.tab_dispatch.lock() {
            map.clear();
        }

        // Rebuild from scratch.
        if list.tabs.is_empty() {
            let placeholder = MenuItem::new("No Claude sessions", false, None);
            if let Err(e) = self.menu.insert(&placeholder, DYNAMIC_OFFSET) {
                error!("Failed to insert placeholder: {}", e);
            }
            self.empty_placeholder = Some(placeholder);
            return;
        }

        for (idx, tab) in list.tabs.iter().enumerate() {
            let label = format_tab_menu_label(tab, list.active_wrapper_id.as_deref());
            let item = MenuItem::new(label, true, None);
            if let Ok(mut map) = self.tab_dispatch.lock() {
                map.insert(item.id().clone(), tab.wrapper_id.clone());
            }
            if let Err(e) = self.menu.insert(&item, DYNAMIC_OFFSET + idx) {
                error!("Failed to insert tab item {}: {}", idx, e);
            }
            self.tab_items.push(item);
        }
    }

    /// Update tray to reflect device presence state
    pub fn set_device_status(
        &mut self,
        presence: DevicePresence,
        device_name: Option<&str>,
        firmware: Option<&str>,
    ) {
        let icon = match presence {
            DevicePresence::Active => self.icons.connected(),
            DevicePresence::Available => self.icons.connected(),
            DevicePresence::None => self.icons.disconnected(),
        };

        let tooltip = match presence {
            DevicePresence::Active => {
                format!("Core Deck Daemon - {}", device_name.unwrap_or("Active"))
            }
            DevicePresence::Available => {
                format!("Core Deck Daemon - {} (idle)", device_name.unwrap_or("Available"))
            }
            DevicePresence::None => "Core Deck Daemon - No device".to_string(),
        };

        if let Err(e) = self.tray.set_icon(Some(icon.clone())) {
            error!("Failed to set tray icon: {}", e);
        }
        if let Err(e) = self.tray.set_tooltip(Some(&tooltip)) {
            error!("Failed to set tray tooltip: {}", e);
        }

        // Top-of-menu device info lines.
        let name_label = match presence {
            DevicePresence::Active => device_name.unwrap_or("Core Deck").to_string(),
            DevicePresence::Available => {
                format!("{} (idle)", device_name.unwrap_or("Core Deck"))
            }
            DevicePresence::None => "No device".to_string(),
        };
        self.device_name_item.set_text(name_label);

        let firmware_label = match firmware {
            Some(fw) if !fw.is_empty() => format!("Firmware {fw}"),
            _ => "Firmware —".to_string(),
        };
        self.device_firmware_item.set_text(firmware_label);
    }
}

/// Format a single tab as a menu label. The active wrapper gets a "● "
/// prefix; everything else gets a leading bullet space so the entries
/// line up. A short status hint (current task or "idle") is appended
/// when available.
fn format_tab_menu_label(tab: &WrapperTab, active_id: Option<&str>) -> String {
    let is_active = active_id == Some(tab.wrapper_id.as_str());
    let bullet = if is_active { "● " } else { "  " };

    let name = crate::wrapper::tab_label(tab);
    let status = tab
        .current_task
        .clone()
        .or_else(|| if tab.active { Some("working".to_string()) } else { None });

    match status {
        Some(s) => format!("{bullet}{name}  —  {s}"),
        None => format!("{bullet}{name}"),
    }
}

// ── Tray icons ─────────────────────────────────────────────────────

const CONNECTED_DARK_DATA: &[u8] = include_bytes!("../assets/icons/tray_connected.png");
const DISCONNECTED_DARK_DATA: &[u8] = include_bytes!("../assets/icons/tray_disconnected.png");
const CONNECTED_LIGHT_DATA: &[u8] = include_bytes!("../assets/icons/tray_connected_light.png");
const DISCONNECTED_LIGHT_DATA: &[u8] = include_bytes!("../assets/icons/tray_disconnected_light.png");

struct TrayIcons {
    connected_dark: tray_icon::Icon,
    disconnected_dark: tray_icon::Icon,
    connected_light: tray_icon::Icon,
    disconnected_light: tray_icon::Icon,
}

impl TrayIcons {
    fn new() -> Result<Self> {
        Ok(Self {
            connected_dark: load_icon_from_png(CONNECTED_DARK_DATA)?,
            disconnected_dark: load_icon_from_png(DISCONNECTED_DARK_DATA)?,
            connected_light: load_icon_from_png(CONNECTED_LIGHT_DATA)?,
            disconnected_light: load_icon_from_png(DISCONNECTED_LIGHT_DATA)?,
        })
    }

    fn connected(&self) -> &tray_icon::Icon {
        if is_dark_mode() { &self.connected_dark } else { &self.connected_light }
    }

    fn disconnected(&self) -> &tray_icon::Icon {
        if is_dark_mode() { &self.disconnected_dark } else { &self.disconnected_light }
    }
}

#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn is_dark_mode() -> bool {
    use cocoa::base::{id, nil};
    use cocoa::foundation::NSString;
    use objc::{msg_send, sel, sel_impl};

    unsafe {
        let user_defaults: id = msg_send![objc::class!(NSUserDefaults), standardUserDefaults];
        let key = NSString::alloc(nil).init_str("AppleInterfaceStyle");
        let value: id = msg_send![user_defaults, stringForKey: key];
        if value == nil {
            false
        } else {
            let utf8: *const i8 = msg_send![value, UTF8String];
            if utf8.is_null() {
                false
            } else {
                let style = std::ffi::CStr::from_ptr(utf8).to_string_lossy();
                style == "Dark"
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn is_dark_mode() -> bool {
    true
}

fn load_icon_from_png(data: &[u8]) -> Result<tray_icon::Icon> {
    let decoder = png::Decoder::new(std::io::Cursor::new(data));
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    buf.truncate(info.buffer_size());

    let rgba_data = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => {
            let mut rgba = Vec::with_capacity(buf.len() * 4 / 3);
            for chunk in buf.chunks(3) {
                rgba.extend_from_slice(chunk);
                rgba.push(255);
            }
            rgba
        }
        png::ColorType::GrayscaleAlpha => {
            let mut rgba = Vec::with_capacity(buf.len() * 2);
            for chunk in buf.chunks(2) {
                rgba.extend_from_slice(&[chunk[0], chunk[0], chunk[0], chunk[1]]);
            }
            rgba
        }
        png::ColorType::Grayscale => {
            let mut rgba = Vec::with_capacity(buf.len() * 4);
            for &gray in &buf {
                rgba.extend_from_slice(&[gray, gray, gray, 255]);
            }
            rgba
        }
        png::ColorType::Indexed => {
            anyhow::bail!("Indexed color not supported");
        }
    };

    tray_icon::Icon::from_rgba(rgba_data, info.width, info.height)
        .map_err(|e| anyhow::anyhow!("Failed to create icon: {}", e))
}

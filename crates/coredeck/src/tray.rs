//! Daemon tray icon and menu
//!
//! Simplified tray for the daemon: device status, Show/Hide app, Quit.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use coredeck_protocol::{WrapperTab, WrapperTabList, TAB_STATE_WORKING};
use tracing::{debug, error, info};
use tray_icon::{
    menu::{IconMenuItem, IsMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
    TrayIcon as TrayIconHandle, TrayIconBuilder,
};

use crate::state::UpdateInfo;

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
    /// Open the settings page at its Setup section (row shown only while
    /// hooks, command-line tools or start-at-login are missing).
    FinishSetup,
    /// Open a URL in the user's default browser. Used by the daemon /
    /// firmware "Update available" rows.
    OpenUrl(String),
    /// Quit the daemon
    Quit,
}

/// Daemon tray manager
pub struct DaemonTrayManager {
    tray: TrayIconHandle,
    icons: TrayIcons,
    /// Last device presence, so the icon can be re-picked when the
    /// update badge toggles.
    presence: DevicePresence,
    /// An "Update available" row is showing — the icon gets an orange dot.
    update_badge: bool,
    menu: Menu,
    /// Disabled top-of-menu items showing the connected device.
    device_name_item: MenuItem,
    device_firmware_item: MenuItem,
    /// Dynamic per-wrapper tab entries — rebuilt when the session list
    /// changes, updated in place otherwise.
    /// Kept owned here so their menu IDs stay valid for click dispatch.
    tab_items: Vec<MenuItem>,
    /// What each entry of `tab_items` currently shows, so ticks that
    /// change only text update the rows in place.
    tab_rows: Vec<TabRow>,
    /// Disabled placeholder shown when no wrappers are connected.
    empty_placeholder: Option<MenuItem>,
    /// MenuId → wrapper_id, used by the event thread to translate clicks
    /// on dynamic tab entries into `FocusWrapper` actions.
    tab_dispatch: Arc<Mutex<HashMap<MenuId, String>>>,
    /// "Install Claude Code hooks…" menu item, present only when hooks
    /// aren't installed in ~/.claude/settings.json.
    setup_item: Option<MenuItem>,
    /// MenuId of the install-hooks item when present, so the event
    /// thread can recognise its click.
    setup_item_id: Arc<Mutex<Option<MenuId>>>,
    /// "Update available: daemon vX.Y.Z" menu item — shown when the
    /// poll task in `updates.rs` finds a newer release tag than the
    /// running binary's `CARGO_PKG_VERSION`.
    daemon_update_item: Option<IconMenuItem>,
    /// "Update available: firmware vX.Y.Z" menu item — same idea, but
    /// against the device-reported firmware version.
    firmware_update_item: Option<IconMenuItem>,
    /// MenuId → release URL for the two update rows. Read by the menu
    /// event thread so a click on either row turns into an
    /// `OpenUrl(release_page)` action.
    update_dispatch: Arc<Mutex<HashMap<MenuId, String>>>,
}

impl DaemonTrayManager {
    pub fn new() -> Result<(Self, std::sync::mpsc::Receiver<DaemonTrayAction>)> {
        let icons = TrayIcons::new().context("Failed to load tray icons")?;

        let menu = Menu::new();

        // Static "About" header showing the daemon version. Disabled
        // (informational only); the firmware version sits on its own
        // line below the device-name row.
        let about_item = MenuItem::new(
            format!("CoreDeck v{}", env!("CARGO_PKG_VERSION")),
            false,
            None,
        );
        let device_name_item = MenuItem::new("No device", false, None);
        let device_firmware_item = MenuItem::new("Firmware —", false, None);

        let settings_item = MenuItem::new("Open Settings…", true, None);
        let settings_id = settings_item.id().clone();

        let quit_item = MenuItem::new("Quit Daemon", true, None);
        let quit_id = quit_item.id().clone();

        // Initial layout: [about, device name, firmware, separator, empty
        // placeholder, separator, Settings, Quit]. Tab entries replace
        // the placeholder on the first `set_tabs` call.
        let empty = MenuItem::new("No Claude sessions", false, None);
        menu.append(&about_item)?;
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
            .with_icon(icons.disconnected(false).clone())
            .build()
            .context("Failed to create tray icon")?;

        info!("Daemon tray icon created");

        let (action_tx, action_rx) = std::sync::mpsc::channel();
        let tab_dispatch: Arc<Mutex<HashMap<MenuId, String>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let setup_item_id: Arc<Mutex<Option<MenuId>>> = Arc::new(Mutex::new(None));
        let update_dispatch: Arc<Mutex<HashMap<MenuId, String>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Menu event handler thread
        let quit_id_clone = quit_id.clone();
        let settings_id_clone = settings_id.clone();
        let tab_dispatch_clone = Arc::clone(&tab_dispatch);
        let setup_item_id_clone = Arc::clone(&setup_item_id);
        let update_dispatch_clone = Arc::clone(&update_dispatch);
        std::thread::spawn(move || {
            let receiver = MenuEvent::receiver();
            loop {
                if let Ok(event) = receiver.recv() {
                    debug!("Daemon menu event: {:?}", event);
                    let setup_item_match = setup_item_id_clone
                        .lock()
                        .ok()
                        .and_then(|g| g.clone())
                        .map(|id| id == event.id)
                        .unwrap_or(false);
                    let update_url = update_dispatch_clone
                        .lock()
                        .ok()
                        .and_then(|m| m.get(&event.id).cloned());
                    let action = if event.id == quit_id_clone {
                        Some(DaemonTrayAction::Quit)
                    } else if event.id == settings_id_clone {
                        Some(DaemonTrayAction::OpenSettings)
                    } else if setup_item_match {
                        Some(DaemonTrayAction::FinishSetup)
                    } else if let Some(url) = update_url {
                        Some(DaemonTrayAction::OpenUrl(url))
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
            presence: DevicePresence::None,
            update_badge: false,
            menu,
            device_name_item,
            device_firmware_item,
            tab_items: Vec::new(),
            tab_rows: Vec::new(),
            empty_placeholder: Some(empty),
            tab_dispatch,
            setup_item: None,
            setup_item_id,
            daemon_update_item: None,
            firmware_update_item: None,
            update_dispatch,
        };

        Ok((manager, action_rx))
    }

    /// Show `list` in the dynamic tab section, the active wrapper marked
    /// with a dot. When the list is empty, restore a disabled "No Claude
    /// sessions" placeholder.
    pub fn set_tabs(&mut self, list: &WrapperTabList) {
        // Insert below the device-info section:
        // [about, name, firmware, separator, ...].
        const DYNAMIC_OFFSET: usize = 4;

        let rows: Vec<TabRow> = list
            .tabs
            .iter()
            .map(|tab| {
                let (title, subtitle, active) =
                    format_tab_menu_label(tab, list.active_wrapper_id.as_deref());
                TabRow {
                    wrapper_id: tab.wrapper_id.clone(),
                    title,
                    subtitle,
                    active,
                }
            })
            .collect();

        if rows.is_empty() && self.tab_rows.is_empty() && self.empty_placeholder.is_some() {
            return;
        }

        // Same sessions in the same order: update the rows in place. The
        // 1 Hz ticker refreshes elapsed times while the menu may be open,
        // and removing/re-inserting items there makes AppKit move the
        // highlight off the hovered item onto a session row, misdrawn.
        let same_sessions = !rows.is_empty()
            && rows.len() == self.tab_rows.len()
            && rows
                .iter()
                .zip(&self.tab_rows)
                .all(|(new, old)| new.wrapper_id == old.wrapper_id);
        if same_sessions {
            let mut changed = Vec::new();
            for (i, (new, old)) in rows.iter().zip(&self.tab_rows).enumerate() {
                if new.title != old.title {
                    self.tab_items[i].set_text(&new.title);
                }
                if new.subtitle != old.subtitle || new.active != old.active {
                    changed.push((DYNAMIC_OFFSET + i, new.subtitle.as_deref(), new.active));
                }
            }
            decorate_tab_rows(&self.menu, &changed);
            self.tab_rows = rows;
            return;
        }

        // Remove existing dynamic items (placeholder or previous tabs).
        if let Some(item) = self.empty_placeholder.take() {
            let _ = self.menu.remove(&item as &dyn IsMenuItem);
        }
        for item in self.tab_items.drain(..) {
            let _ = self.menu.remove(&item as &dyn IsMenuItem);
        }
        self.tab_rows.clear();
        if let Ok(mut map) = self.tab_dispatch.lock() {
            map.clear();
        }

        // Rebuild from scratch.
        if rows.is_empty() {
            let placeholder = MenuItem::new("No Claude sessions", false, None);
            if let Err(e) = self.menu.insert(&placeholder, DYNAMIC_OFFSET) {
                error!("Failed to insert placeholder: {}", e);
            }
            self.empty_placeholder = Some(placeholder);
            return;
        }

        let mut decorations = Vec::with_capacity(rows.len());
        for (idx, row) in rows.iter().enumerate() {
            let item = MenuItem::new(&row.title, true, None);
            if let Ok(mut map) = self.tab_dispatch.lock() {
                map.insert(item.id().clone(), row.wrapper_id.clone());
            }
            if let Err(e) = self.menu.insert(&item, DYNAMIC_OFFSET + idx) {
                error!("Failed to insert tab item {}: {}", idx, e);
            }
            self.tab_items.push(item);
            decorations.push((DYNAMIC_OFFSET + idx, row.subtitle.as_deref(), row.active));
        }
        decorate_tab_rows(&self.menu, &decorations);
        self.tab_rows = rows;
    }

    /// Set the tray icon for the current presence, badged with an orange
    /// dot while an update row is showing.
    fn refresh_icon(&self) {
        let icon = match self.presence {
            DevicePresence::Active | DevicePresence::Available => {
                self.icons.connected(self.update_badge)
            }
            DevicePresence::None => self.icons.disconnected(self.update_badge),
        };
        if let Err(e) = self.tray.set_icon(Some(icon.clone())) {
            error!("Failed to set tray icon: {}", e);
        }
    }

    /// Update tray to reflect device presence state
    pub fn set_device_status(
        &mut self,
        presence: DevicePresence,
        device_name: Option<&str>,
        firmware: Option<&str>,
    ) {
        self.presence = presence;
        self.refresh_icon();

        let tooltip = match presence {
            DevicePresence::Active => {
                format!("Core Deck Daemon - {}", device_name.unwrap_or("Active"))
            }
            DevicePresence::Available => {
                format!(
                    "Core Deck Daemon - {} (idle)",
                    device_name.unwrap_or("Available")
                )
            }
            DevicePresence::None => "Core Deck Daemon - No device".to_string(),
        };

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

    /// Show or hide the "Finish setup…" menu row depending on whether
    /// setup (hooks, command-line tools, start-at-login) is complete. Sits
    /// just above the Settings/Quit pair so the user discovers it the
    /// first time they open the menu after a fresh install.
    pub fn set_setup_complete(&mut self, complete: bool) {
        if complete {
            if let Some(item) = self.setup_item.take() {
                let _ = self.menu.remove(&item as &dyn IsMenuItem);
                if let Ok(mut g) = self.setup_item_id.lock() {
                    *g = None;
                }
            }
            return;
        }
        if self.setup_item.is_some() {
            return;
        }
        let item = MenuItem::new("⚠ Finish setup…", true, None);
        if let Ok(mut g) = self.setup_item_id.lock() {
            *g = Some(item.id().clone());
        }
        let position = self.setup_item_insert_position();
        if let Err(e) = self.menu.insert(&item, position) {
            error!("Failed to insert finish-setup item: {}", e);
            return;
        }
        self.setup_item = Some(item);
    }

    /// Replace the "Update available" rows for the daemon and firmware.
    /// Either side may be `None` to indicate "no update". The poll task
    /// in `updates.rs` calls this with both sides on every refresh, so
    /// we just rebuild the section from scratch each time. Update rows
    /// sit between the wrapper-tab section and the "Install hooks" /
    /// Settings rows.
    pub fn set_updates(&mut self, daemon: Option<UpdateInfo>, firmware: Option<UpdateInfo>) {
        // Tear down whatever's currently in the update slot.
        if let Some(item) = self.daemon_update_item.take() {
            let _ = self.menu.remove(&item as &dyn IsMenuItem);
        }
        if let Some(item) = self.firmware_update_item.take() {
            let _ = self.menu.remove(&item as &dyn IsMenuItem);
        }
        if let Ok(mut map) = self.update_dispatch.lock() {
            map.clear();
        }

        // Re-insert from the base position upward. Each successful insert
        // shifts everything below by one, so we step the position too.
        self.update_badge = daemon.is_some() || firmware.is_some();
        self.refresh_icon();

        let mut pos = self.extras_base_position();
        if let Some(info) = daemon {
            let label = format!("Update available: daemon v{}", info.latest_version);
            let item = IconMenuItem::new(label, true, update_dot_menu_icon(), None);
            if let Ok(mut map) = self.update_dispatch.lock() {
                map.insert(item.id().clone(), info.html_url);
            }
            if let Err(e) = self.menu.insert(&item, pos) {
                error!("Failed to insert daemon update item: {}", e);
            } else {
                pos += 1;
                self.daemon_update_item = Some(item);
            }
        }
        if let Some(info) = firmware {
            let label = format!("Update available: firmware v{}", info.latest_version);
            let item = IconMenuItem::new(label, true, update_dot_menu_icon(), None);
            if let Ok(mut map) = self.update_dispatch.lock() {
                map.insert(item.id().clone(), info.html_url);
            }
            if let Err(e) = self.menu.insert(&item, pos) {
                error!("Failed to insert firmware update item: {}", e);
            } else {
                self.firmware_update_item = Some(item);
            }
        }
    }

    /// First slot in the "extras" section that lives between the
    /// second separator and the trailing Settings/Quit pair. Update
    /// rows are inserted here, pushing any existing install-hooks /
    /// settings rows down.
    fn extras_base_position(&self) -> usize {
        // Layout: [about, name, firmware, sep, dynamic_tabs|placeholder, sep,
        //          ...extras..., settings, quit]
        let dynamic = if self.tab_items.is_empty() {
            if self.empty_placeholder.is_some() {
                1
            } else {
                0
            }
        } else {
            self.tab_items.len()
        };
        // 4 = about + name + firmware + first separator. +1 = second separator.
        4 + dynamic + 1
    }

    /// Insertion point for the "Install hooks" row — sits below any
    /// active update rows.
    fn setup_item_insert_position(&self) -> usize {
        self.extras_base_position()
            + (self.daemon_update_item.is_some() as usize)
            + (self.firmware_update_item.is_some() as usize)
    }
}

/// One session row as shown in the menu.
struct TabRow {
    wrapper_id: String,
    title: String,
    subtitle: Option<String>,
    active: bool,
}

/// Build the (title, subtitle, is_active) tuple for a single tab row.
/// The title is just the session name — alignment between rows comes
/// from NSMenuItem's built-in state column (a checkmark for the active
/// row), not a leading bullet character, so the title and subtitle
/// always start at the same x position. The subtitle is the current
/// task (or "working" if the session is in WORKING state with no task
/// string), rendered by macOS 14.4+'s `NSMenuItem.subtitle`. On older
/// macOS it's silently dropped.
fn format_tab_menu_label(
    tab: &WrapperTab,
    active_id: Option<&str>,
) -> (String, Option<String>, bool) {
    let is_active = active_id == Some(tab.wrapper_id.as_str());
    let name = crate::wrapper::tab_label_long(tab);
    let subtitle = tab.current_task.clone().or_else(|| {
        if tab.tab_state == TAB_STATE_WORKING {
            Some("working".to_string())
        } else {
            None
        }
    });
    (name, subtitle, is_active)
}

/// Decorate tab rows' NSMenuItems — `(menu index, subtitle, active)`
/// each — with a subtitle (macOS 14.4+'s `setSubtitle:`) and a state
/// indicator (`setState:` for the active row). The state column gives consistent left-alignment across rows
/// without needing a leading bullet character in the title — important
/// because proportional menu fonts mean a literal "● " vs. "  " prefix
/// don't line up to the same x. Walks the underlying NSMenu via
/// `muda::Menu::ns_menu()`. On older macOS where `setSubtitle:` isn't
/// supported, the subtitle is dropped but the state column still
/// works. No-op on non-macOS.
#[cfg(target_os = "macos")]
fn decorate_tab_rows(menu: &Menu, rows: &[(usize, Option<&str>, bool)]) {
    use cocoa::base::{id, nil};
    use cocoa::foundation::{NSPoint, NSRect, NSSize, NSString};
    use objc::{class, msg_send, sel, sel_impl};
    // Pulls `Menu::ns_menu()` into scope on macOS — the symbol is
    // exposed only via the ContextMenu trait, so an explicit import
    // is required even though we never name `ContextMenu` itself.
    use tray_icon::menu::ContextMenu;

    if rows.is_empty() {
        return;
    }
    let ptr = menu.ns_menu();
    if ptr.is_null() {
        return;
    }
    // NSControlStateValueOn = 1, Off = 0.
    const STATE_ON: i64 = 1;
    const STATE_OFF: i64 = 0;
    unsafe {
        let ns_menu = ptr as id;
        let item_array: id = msg_send![ns_menu, itemArray];
        if item_array == nil {
            return;
        }
        let count: usize = msg_send![item_array, count];

        // `setSubtitle:` is macOS 14.4+; `setState:` has been around
        // since 10.0. Probe the subtitle selector and skip just that
        // call on older systems.
        let subtitle_supported: bool = {
            let resp: i8 = msg_send![
                class!(NSMenuItem),
                instancesRespondToSelector: sel!(setSubtitle:)
            ];
            resp != 0
        };

        // Two flavours of the active-row indicator:
        //   - `tall`: 14×32 with the dot biased toward the top, used
        //     for two-line rows (title + subtitle). AppKit centers the
        //     state image vertically in the row; the upward bias inside
        //     the canvas lands the dot on the title line (centered on
        //     its lowercase letters, like a bullet) rather than between
        //     the lines.
        //   - `short`: 14×14 with the dot centered, used for one-line
        //     rows. AppKit centers it; we just want a centered dot.
        // Without the split, single-line rows would inherit the
        // tall canvas and AppKit's centering would put the dot above
        // the title text.
        let tall = make_active_indicator_image(IndicatorVariant::TwoLine);
        let short = make_active_indicator_image(IndicatorVariant::OneLine);

        for &(idx, sub, on) in rows {
            if idx >= count {
                break;
            }
            let item: id = msg_send![item_array, objectAtIndex: idx];
            if item == nil {
                continue;
            }
            let state: i64 = if on { STATE_ON } else { STATE_OFF };
            let _: () = msg_send![item, setState: state];
            let has_subtitle = sub.is_some_and(|s| !s.is_empty());
            let on_image = if has_subtitle && subtitle_supported {
                tall
            } else {
                short
            };
            // Set on inactive rows too: which variant fits depends on
            // the subtitle, not on the state.
            let _: () = msg_send![item, setOnStateImage: on_image];

            if subtitle_supported {
                match sub {
                    Some(s) if !s.is_empty() => {
                        let ns: id = NSString::alloc(nil).init_str(s);
                        let _: () = msg_send![item, setSubtitle: ns];
                        // `subtitle` is a copy property; drop our +1.
                        let _: () = msg_send![ns, release];
                    }
                    _ => {
                        let _: () = msg_send![item, setSubtitle: nil];
                    }
                }
            }
        }

        // The menu items each retain whichever image they got via
        // setOnStateImage:; release our local +1 from `alloc/init`.
        let _: () = msg_send![tall, release];
        let _: () = msg_send![short, release];
    }

    enum IndicatorVariant {
        OneLine,
        TwoLine,
    }

    /// Build the filled-circle NSImage used as the active-row
    /// indicator. Caller owns one +1 retain (from `alloc/init`);
    /// release after handing to AppKit. See the call site for why
    /// two variants exist.
    unsafe fn make_active_indicator_image(variant: IndicatorVariant) -> id {
        let nsimage_class = class!(NSImage);
        let image: id = msg_send![nsimage_class, alloc];

        // Image coords are non-flipped (Y-up). For TwoLine we use a
        // tall canvas (32) and bias the dot 6pt above center so AppKit's
        // vertical centering lands it on the title line (8pt put it at
        // cap height, visibly above the lowercase text); for OneLine we
        // use a square 14×14 canvas with the dot centered.
        let (size, circle_origin) = match variant {
            IndicatorVariant::TwoLine => (NSSize::new(14.0, 32.0), NSPoint::new(3.0, 18.0)),
            IndicatorVariant::OneLine => (NSSize::new(14.0, 14.0), NSPoint::new(3.0, 3.0)),
        };
        let image: id = msg_send![image, initWithSize: size];

        let _: () = msg_send![image, lockFocus];

        // labelColor adapts to light/dark and respects accessibility
        // settings — same source the system ✓ uses.
        let color: id = msg_send![class!(NSColor), labelColor];
        let _: () = msg_send![color, set];

        let circle_rect = NSRect::new(circle_origin, NSSize::new(8.0, 8.0));
        let path: id = msg_send![
            class!(NSBezierPath),
            bezierPathWithOvalInRect: circle_rect
        ];
        let _: () = msg_send![path, fill];

        let _: () = msg_send![image, unlockFocus];
        image
    }
}

#[cfg(not(target_os = "macos"))]
fn decorate_tab_rows(_menu: &Menu, _rows: &[(usize, Option<&str>, bool)]) {}

// ── Tray icons ─────────────────────────────────────────────────────

const CONNECTED_DARK_DATA: &[u8] = include_bytes!("../assets/icons/tray_connected.png");
const DISCONNECTED_DARK_DATA: &[u8] = include_bytes!("../assets/icons/tray_disconnected.png");
const CONNECTED_LIGHT_DATA: &[u8] = include_bytes!("../assets/icons/tray_connected_light.png");
const DISCONNECTED_LIGHT_DATA: &[u8] =
    include_bytes!("../assets/icons/tray_disconnected_light.png");

struct TrayIcons {
    connected_dark: tray_icon::Icon,
    disconnected_dark: tray_icon::Icon,
    connected_light: tray_icon::Icon,
    disconnected_light: tray_icon::Icon,
    /// Same four, with the orange "update available" dot.
    connected_dark_badged: tray_icon::Icon,
    disconnected_dark_badged: tray_icon::Icon,
    connected_light_badged: tray_icon::Icon,
    disconnected_light_badged: tray_icon::Icon,
}

impl TrayIcons {
    fn new() -> Result<Self> {
        let (connected_dark, connected_dark_badged) = load_icon_pair(CONNECTED_DARK_DATA)?;
        let (disconnected_dark, disconnected_dark_badged) = load_icon_pair(DISCONNECTED_DARK_DATA)?;
        let (connected_light, connected_light_badged) = load_icon_pair(CONNECTED_LIGHT_DATA)?;
        let (disconnected_light, disconnected_light_badged) =
            load_icon_pair(DISCONNECTED_LIGHT_DATA)?;
        Ok(Self {
            connected_dark,
            disconnected_dark,
            connected_light,
            disconnected_light,
            connected_dark_badged,
            disconnected_dark_badged,
            connected_light_badged,
            disconnected_light_badged,
        })
    }

    fn connected(&self, badged: bool) -> &tray_icon::Icon {
        match (is_dark_mode(), badged) {
            (true, false) => &self.connected_dark,
            (true, true) => &self.connected_dark_badged,
            (false, false) => &self.connected_light,
            (false, true) => &self.connected_light_badged,
        }
    }

    fn disconnected(&self, badged: bool) -> &tray_icon::Icon {
        match (is_dark_mode(), badged) {
            (true, false) => &self.disconnected_dark,
            (true, true) => &self.disconnected_dark_badged,
            (false, false) => &self.disconnected_light,
            (false, true) => &self.disconnected_light_badged,
        }
    }
}

/// Core Deck orange (the Agent button, `#FF6600`).
const UPDATE_DOT_RGB: [u8; 3] = [0xFF, 0x66, 0x00];

/// Paint a filled `UPDATE_DOT_RGB` circle into an RGBA buffer, with an
/// optional transparent ring around it (`gap`) so it separates from
/// whatever it overlaps. Edges are anti-aliased by pixel coverage.
fn paint_dot(rgba: &mut [u8], width: u32, cx: f32, cy: f32, radius: f32, gap: f32) {
    let height = rgba.len() as u32 / 4 / width;
    for y in 0..height {
        for x in 0..width {
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            let i = ((y * width + x) * 4) as usize;
            let dot = (radius + 0.5 - d).clamp(0.0, 1.0);
            if dot > 0.0 {
                let a = rgba[i + 3] as f32 / 255.0;
                for c in 0..3 {
                    let under = rgba[i + c] as f32;
                    rgba[i + c] = (UPDATE_DOT_RGB[c] as f32 * dot + under * (1.0 - dot)) as u8;
                }
                rgba[i + 3] = ((dot + a * (1.0 - dot)) * 255.0) as u8;
            } else if gap > 0.0 {
                // Knock the glyph out in a thin ring around the dot.
                let clear = (radius + gap + 0.5 - d).clamp(0.0, 1.0);
                rgba[i + 3] = (rgba[i + 3] as f32 * (1.0 - clear)) as u8;
            }
        }
    }
}

/// Add the update dot to a tray icon: top-right corner, ~30% of the icon
/// across, with a thin knocked-out ring so it reads over the glyph.
fn badge_rgba(rgba: &mut [u8], width: u32) {
    let r = width as f32 * 0.15;
    let edge = width as f32 - r - 1.0;
    paint_dot(rgba, width, edge, r + 1.0, r, width as f32 * 0.06);
}

/// Plain and orange-dot-badged versions of a tray icon PNG.
fn load_icon_pair(data: &[u8]) -> Result<(tray_icon::Icon, tray_icon::Icon)> {
    let (rgba, width, height) = decode_png_rgba(data)?;
    let mut badged = rgba.clone();
    badge_rgba(&mut badged, width);
    let icon = |buf| {
        tray_icon::Icon::from_rgba(buf, width, height)
            .map_err(|e| anyhow::anyhow!("Failed to create icon: {}", e))
    };
    Ok((icon(rgba)?, icon(badged)?))
}

/// The orange dot shown on "Update available" menu rows.
fn update_dot_menu_icon() -> Option<tray_icon::menu::Icon> {
    const SIZE: u32 = 32;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    let c = SIZE as f32 / 2.0;
    paint_dot(&mut rgba, SIZE, c, c, SIZE as f32 * 0.22, 0.0);
    tray_icon::menu::Icon::from_rgba(rgba, SIZE, SIZE).ok()
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

fn decode_png_rgba(data: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
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

    Ok((rgba_data, info.width, info.height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_is_an_orange_dot_in_the_top_right_corner() {
        let (plain, width, _) = decode_png_rgba(CONNECTED_DARK_DATA).unwrap();
        let mut badged = plain.clone();
        badge_rgba(&mut badged, width);
        let px = |buf: &[u8], x: u32, y: u32| {
            let i = ((y * width + x) * 4) as usize;
            [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
        };
        let r = width as f32 * 0.15;
        let (cx, cy) = ((width as f32 - r - 1.0) as u32, (r + 1.0) as u32);
        assert_eq!(px(&badged, cx, cy), [0xFF, 0x66, 0x00, 0xFF]);
        // Away from the corner the icon is untouched.
        assert_eq!(px(&badged, 2, width - 3), px(&plain, 2, width - 3));
    }
}

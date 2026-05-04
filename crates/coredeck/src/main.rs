//! CoreDeck Daemon - Background process that owns the HID device
//!
//! Provides WebSocket (exclusive) and HTTP REST (shared) APIs for
//! controlling the CoreDeck macropad.

mod alerts;
mod hid;
mod hooks;
mod keymap;
mod presets;
mod raise;
mod rpc;
mod state;
mod tray;
mod wrapper;
mod ws;

use coredeck_protocol::DEFAULT_DAEMON_ADDR;
use clap::{Parser, Subcommand};
use hid::HidManager;
use state::{ClaudeState, DaemonEvent, DaemonEventSender, DeviceStatus, TrayUpdate, Wrapper};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, Notify, RwLock};
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// HID device configuration (matches the app's HidConfig)
#[derive(Debug, Clone)]
pub struct HidConfig {
    pub vendor_id: u16,
    pub product_id: u16,
    pub usage_page: u16,
    pub usage_id: u16,
    pub ping_interval_ms: u64,
    pub reconnect_interval_ms: u64,
}

impl Default for HidConfig {
    fn default() -> Self {
        Self {
            vendor_id: 0xFEED,
            product_id: 0x0803,
            usage_page: 0xFF60,
            usage_id: 0x61,
            ping_interval_ms: 5000,
            reconnect_interval_ms: 2000,
        }
    }
}

/// Shared state across the daemon (must be Send + Sync for axum)
pub struct DaemonState {
    /// HID device manager
    pub hid: Mutex<HidManager>,
    /// Current device status
    pub device_status: RwLock<DeviceStatus>,
    /// Connected WS client (the lock)
    pub ws_client: Mutex<Option<ws::WsClientHandle>>,
    /// Notified when WS lock changes
    pub notify_lock_change: Notify,
    /// Channel to send tray updates to the main thread (tray is !Send, lives on main thread)
    pub tray_tx: std::sync::mpsc::Sender<TrayUpdate>,
    /// State derived from Claude Code hooks
    pub claude_state: RwLock<ClaudeState>,
    /// Active `coredeck-claude` wrappers, keyed by wrapper_id
    pub wrappers: RwLock<HashMap<String, Wrapper>>,
    /// What's currently displayed on the device's alert overlay (idle
    /// notification / pending permission decision / nothing).
    pub alert_state: Mutex<alerts::AlertState>,
    /// Pending permission alerts that arrived while another alert was
    /// already showing. Popped when the active alert resolves so the
    /// user doesn't miss prompts from parallel Claude sessions.
    pub pending_queue: Mutex<std::collections::VecDeque<alerts::QueuedPending>>,
    /// Wrapper IDs that have explicitly opted in to Auto-approve
    /// under the current global Auto-approve session. Populated on
    /// Auto-approve ON (active wrapper auto-opts in) and on Allow of
    /// the per-wrapper "Auto-approve this tab?" enrollment alert.
    /// Cleared wholesale on Auto-approve OFF and HID disconnect;
    /// per-wrapper entries drop on wrapper disconnect.
    pub yolo_opt_in: Mutex<std::collections::HashSet<String>>,
    /// Wrapper IDs that have explicitly DECLINED enrollment via Deny
    /// on the "Auto-approve this tab?" alert. Suppresses re-prompting
    /// — the daemon falls back to Claude's terminal prompt for every
    /// PermissionRequest from these wrappers until Auto-approve
    /// toggles, the device disconnects, or the wrapper exits.
    pub yolo_opt_out: Mutex<std::collections::HashSet<String>>,
    /// Listen address (for hooks install to know the URL)
    pub listen_addr: String,
}

impl DaemonState {
    pub fn send_tray_update(&self, update: TrayUpdate) {
        let _ = self.tray_tx.send(update);
    }
}

#[derive(Parser)]
#[command(name = "coredeck", about = "CoreDeck background daemon")]
struct Cli {
    /// Listen address
    #[arg(long, default_value = DEFAULT_DAEMON_ADDR)]
    listen: String,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Install launchd plist for auto-start
    Install,
    /// Uninstall launchd plist
    Uninstall,
    /// Manage Claude Code hooks configuration
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },
    /// One-shot setup: install hooks, register launchd, print alias hint
    Setup,
}

#[derive(Subcommand)]
enum HooksAction {
    /// Install Claude Code hooks in ~/.claude/settings.json
    Install,
    /// Remove Claude Code hooks from ~/.claude/settings.json
    Uninstall,
}

fn main() {
    // Initialize logging
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();

    // Handle subcommands
    match cli.command {
        Some(Commands::Install) => {
            install_launchd(&cli.listen);
            return;
        }
        Some(Commands::Uninstall) => {
            uninstall_launchd();
            return;
        }
        Some(Commands::Hooks { action }) => {
            match action {
                HooksAction::Install => hooks::install_claude_hooks(&cli.listen),
                HooksAction::Uninstall => hooks::uninstall_claude_hooks(),
            }
            return;
        }
        Some(Commands::Setup) => {
            run_setup(&cli.listen);
            return;
        }
        None => {}
    }

    info!("Starting CoreDeck daemon on {}", cli.listen);

    // Install a raw SIGINT handler so Ctrl-C actually terminates the process.
    // On macOS, winit's NSApplication::run() installs its own signal handlers
    // which intercept SIGINT before tokio can see it.
    install_signal_handler();

    // macOS: set activation policy to Accessory (no dock icon, just tray)
    #[cfg(target_os = "macos")]
    setup_macos_accessory();

    // Create event channel for HID events
    let (event_tx, event_rx) = mpsc::unbounded_channel::<DaemonEvent>();
    let initial_event_tx = event_tx.clone();
    let event_sender = DaemonEventSender::new(event_tx);

    // Initialize HID manager
    let hid_config = HidConfig::default();
    let hid_manager = match HidManager::new(hid_config, event_sender) {
        Ok(hid) => {
            info!("HID manager initialized");
            hid
        }
        Err(e) => {
            error!("Failed to initialize HID manager: {}", e);
            std::process::exit(1);
        }
    };

    // Create tray (must happen on main thread on macOS)
    let (tray_manager, tray_action_rx) = match tray::DaemonTrayManager::new() {
        Ok((tray, rx)) => (Some(tray), Some(rx)),
        Err(e) => {
            error!("Failed to create tray: {}", e);
            (None, None)
        }
    };

    // Channel for async code to send tray updates to main thread
    let (tray_update_tx, tray_update_rx) = std::sync::mpsc::channel::<TrayUpdate>();

    // Initialize device status from HID manager's enumeration
    let initially_available = hid_manager.is_device_available();
    let initial_device_name = hid_manager.cached_device_name();
    let initial_status = DeviceStatus {
        available: initially_available,
        device_name: initial_device_name.clone(),
        ..DeviceStatus::default()
    };

    // Build shared state (Send + Sync — no tray handle here)
    let state = Arc::new(DaemonState {
        hid: Mutex::new(hid_manager),
        device_status: RwLock::new(initial_status),
        ws_client: Mutex::new(None),
        notify_lock_change: Notify::new(),
        tray_tx: tray_update_tx,
        claude_state: RwLock::new(ClaudeState::default()),
        wrappers: RwLock::new(HashMap::new()),
        alert_state: Mutex::new(alerts::AlertState::default()),
        pending_queue: Mutex::new(std::collections::VecDeque::new()),
        yolo_opt_in: Mutex::new(std::collections::HashSet::new()),
        yolo_opt_out: Mutex::new(std::collections::HashSet::new()),
        listen_addr: cli.listen.clone(),
    });

    // Emit initial DeviceAvailable event if device was found during enumeration.
    // Hotplug/polling monitors only fire on state *changes*, so they miss
    // a device that was already plugged in when the daemon started.
    if initially_available {
        let name = initial_device_name.unwrap_or_else(|| "Core Deck".to_string());
        info!("Emitting initial DeviceAvailable for already-connected device: {}", name);
        let _ = initial_event_tx.send(DaemonEvent::DeviceAvailable { device_name: name });
    }

    // Seed the tray with the initial hooks-installed state.
    state.send_tray_update(TrayUpdate::HooksInstalled(hooks::are_hooks_installed()));

    // Run the tokio runtime + axum server on a spawned thread.
    // The winit event loop must run on the main thread (required for tray on macOS).
    let state_clone = Arc::clone(&state);
    let listen_addr = cli.listen.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime");

        rt.block_on(async move {
            run_async(state_clone, event_rx, listen_addr).await;
        });

        // run_async returns when Ctrl-C is received. The main thread is blocked
        // in the winit event loop (for tray icon), so exit the process directly.
        info!("Async runtime finished, exiting process");
        std::process::exit(0);
    });

    // Handle tray events on main thread (via winit event loop)
    run_main_event_loop(state, tray_manager, tray_action_rx, tray_update_rx);
}

/// Run the async daemon (axum server + event processing)
async fn run_async(
    state: Arc<DaemonState>,
    mut event_rx: mpsc::UnboundedReceiver<DaemonEvent>,
    listen_addr: String,
) {
    // Build axum router
    let app = axum::Router::new()
        .route("/ws", axum::routing::get(ws::ws_handler))
        .route("/api/status", axum::routing::get(rpc::get_status))
        .route("/api/display", axum::routing::post(rpc::post_display))
        .route("/api/alert", axum::routing::post(rpc::post_alert))
        .route("/api/alert/clear", axum::routing::post(rpc::post_alert_clear))
        .route("/api/brightness", axum::routing::post(rpc::post_brightness))
        .route("/api/mode", axum::routing::post(rpc::post_mode))
        .route("/api/version", axum::routing::get(rpc::get_version))
        .route("/api/hooks/status", axum::routing::get(rpc::get_hooks_status))
        .route("/api/hooks/install", axum::routing::post(rpc::post_hooks_install))
        .route("/api/hooks/uninstall", axum::routing::post(rpc::post_hooks_uninstall))
        .route("/api/soft-keys", axum::routing::get(rpc::get_soft_keys))
        .route("/api/soft-keys/reset", axum::routing::post(rpc::post_soft_keys_reset))
        .route("/api/soft-keys/presets", axum::routing::get(rpc::get_soft_key_presets))
        .route("/api/soft-keys/presets/apply", axum::routing::post(rpc::apply_soft_key_preset))
        .route("/api/soft-keys/presets/save", axum::routing::post(rpc::save_soft_key_preset))
        .route("/api/soft-keys/presets/{name}", axum::routing::delete(rpc::delete_soft_key_preset))
        .route("/api/soft-keys/{index}", axum::routing::put(rpc::put_soft_key))
        .route("/api/wrappers", axum::routing::get(rpc::get_wrappers))
        // Static settings page (HTML embedded in the binary)
        .route("/", axum::routing::get(rpc::get_settings_page))
        .route("/settings", axum::routing::get(rpc::get_settings_page))
        // Claude Code hook endpoints (no WS lock needed — hooks come from Claude Code)
        .route("/hooks/{event_type}", axum::routing::post(hooks::handle_hook))
        // coredeck-claude wrapper protocol
        .route("/wrapper-ws", axum::routing::get(wrapper::wrapper_ws_handler))
        .route("/wrapper/register", axum::routing::post(wrapper::wrapper_register_session))
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any),
        )
        .with_state(Arc::clone(&state));

    // Start HTTP/WS server
    let listener = match tokio::net::TcpListener::bind(&listen_addr).await {
        Ok(l) => {
            info!("Listening on {}", listen_addr);
            l
        }
        Err(e) => {
            error!("Failed to bind to {}: {}", listen_addr, e);
            std::process::exit(1);
        }
    };

    let server = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            error!("Server error: {}", e);
        }
    });

    // 1Hz tab-list ticker — keeps elapsed-seconds advancing on the device
    // between hook events for the active session.
    let ticker_state = Arc::clone(&state);
    tokio::spawn(async move {
        wrapper::run_display_ticker(ticker_state).await;
    });

    // Process HID events and forward to WS client
    let state_for_events = Arc::clone(&state);
    let event_handler = tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            // Update shared device status and notify tray
            match &event {
                DaemonEvent::HidConnected { device_name, firmware_version } => {
                    let mut status = state_for_events.device_status.write().await;
                    status.available = true;
                    status.connected = true;
                    status.device_name = Some(device_name.clone());
                    status.firmware_version = Some(firmware_version.clone());

                    state_for_events.send_tray_update(TrayUpdate::DeviceConnected {
                        name: device_name.clone(),
                        firmware: firmware_version.clone(),
                    });
                }
                DaemonEvent::HidDisconnected => {
                    {
                        let mut status = state_for_events.device_status.write().await;
                        status.connected = false;
                        status.device_name = None;
                        status.firmware_version = None;
                        // Disarm YOLO on disconnect so reconnect doesn't silently
                        // resume auto-approve. Permission gating already requires
                        // `connected`, but clearing the flag keeps device + daemon
                        // state in sync — a fresh connect comes back with the
                        // firmware's StateReport, which will re-arm if the
                        // physical switch is still on.
                        status.yolo = false;
                        status.mode_initialized = false;
                    }
                    // YOLO is gone, so the per-wrapper opt-in set is too.
                    wrapper::clear_yolo_enrollment(&state_for_events).await;

                    state_for_events.send_tray_update(TrayUpdate::DeviceDisconnected);
                }
                DaemonEvent::DeviceAvailable { device_name } => {
                    {
                        let mut status = state_for_events.device_status.write().await;
                        status.available = true;
                        status.device_name = Some(device_name.clone());
                    }
                    state_for_events.send_tray_update(TrayUpdate::DeviceAvailable(device_name.clone()));
                    // Open HID immediately and keep it open for the
                    // daemon's lifetime — no GUI app means there's
                    // nothing to hand it off to, and the open/close
                    // churn from transient HTTP requests was racing
                    // protocol detection and EEPROM saves.
                    let hid = state_for_events.hid.lock().await;
                    if !hid.is_connected() {
                        if let Err(e) = hid.open_device() {
                            warn!("Failed to open HID on DeviceAvailable: {}", e);
                        }
                    }
                }
                DaemonEvent::DeviceUnavailable => {
                    let mut status = state_for_events.device_status.write().await;
                    status.available = false;
                    status.device_name = None;
                    status.firmware_version = None;
                    state_for_events.send_tray_update(TrayUpdate::DeviceUnavailable);
                }
                DaemonEvent::DeviceStateChanged { mode, yolo } => {
                    // Mode-button tap on the device fires this with a new
                    // `mode`. The firmware deliberately swallows the keypress
                    // (rev1.c:103) and expects the daemon to inject Shift+Tab
                    // into the active wrapper so Claude Code cycles modes
                    // too. Skip the very first state report after connect —
                    // that's the initial sync, not a tap.
                    let (inject_shift_tab, yolo_transition) = {
                        let mut status = state_for_events.device_status.write().await;
                        let mode_changed = status.mode != *mode;
                        let was_initialized = status.mode_initialized;
                        let prev_yolo = status.yolo;
                        status.mode = *mode;
                        status.yolo = *yolo;
                        status.mode_initialized = true;
                        // Only treat as a transition once we've seen a prior
                        // state report; the initial sync isn't a flip.
                        let yolo_transition = if was_initialized && prev_yolo != *yolo {
                            Some(*yolo)
                        } else {
                            None
                        };
                        (was_initialized && mode_changed, yolo_transition)
                    };
                    match yolo_transition {
                        Some(true) => {
                            // YOLO turned ON — the wrapper that's focused at
                            // this moment auto-opts in. Other wrappers will
                            // be asked on their first PermissionRequest.
                            if let Some(wid) =
                                wrapper::active_wrapper_id(&state_for_events).await
                            {
                                wrapper::mark_yolo_opt_in(&state_for_events, &wid).await;
                                info!("YOLO ON — wrapper {} auto-opted in", wid);
                            }
                        }
                        Some(false) => {
                            wrapper::clear_yolo_enrollment(&state_for_events).await;
                            info!("YOLO OFF — opt-in set cleared");
                        }
                        None => {}
                    }
                    if inject_shift_tab {
                        if let Err(e) = wrapper::write_to_target(
                            &state_for_events,
                            "",
                            b"\x1b[Z".to_vec(),
                        )
                        .await
                        {
                            debug!(error = %e, "mode-tap Shift+Tab injection failed");
                        }
                    }
                }
                _ => {}
            }

            // Try to consume HID input as a response to a live alert
            // (idle prompt clear, F20 focus, interactive permission
            // decision). Outcomes:
            //   - Passthrough: route to active wrapper as normal
            //   - Consumed: alert resolved; don't route
            //   - FocusSession: F20 with alert up — switch active to the
            //     alerting session, raise its terminal, leave alert up,
            //     don't route
            //   - RaiseActive: F20 with no alert — raise the currently-
            //     active session's terminal, don't route
            if matches!(
                &event,
                DaemonEvent::HidKeyEvent { .. } | DaemonEvent::HidTypeString { .. }
            ) {
                match alerts::consume_input_for_decision(&state_for_events, &event).await {
                    alerts::AlertOutcome::Consumed => continue,
                    alerts::AlertOutcome::FocusSession(sid) => {
                        let _ = wrapper::set_active_for_session(&state_for_events, &sid).await;
                        raise::raise_for_session(&state_for_events, &sid).await;
                        continue;
                    }
                    alerts::AlertOutcome::RaiseActive => {
                        raise::raise_active(&state_for_events).await;
                        continue;
                    }
                    alerts::AlertOutcome::Passthrough => {}
                }
            }

            // Knob press+rotate is a daemon-level cycler (replaces the
            // old "F20 cycles" idea). Firmware emits Ctrl+Tab / Ctrl+
            // Shift+Tab from encoder Layer 1; intercept here so neither
            // the wrapper PTY nor any outer terminal sees a literal tab
            // switch escape.
            if let DaemonEvent::HidKeyEvent { keycode } = &event {
                match *keycode {
                    keymap::KEYCODE_KNOB_NEXT => {
                        wrapper::cycle_active(&state_for_events, 1).await;
                        continue;
                    }
                    keymap::KEYCODE_KNOB_PREV => {
                        wrapper::cycle_active(&state_for_events, -1).await;
                        continue;
                    }
                    _ => {}
                }
            }

            // Route HID input to the active wrapper's PTY when one is connected.
            // Only fall through to legacy WS forwarding (GUI app) when no
            // wrapper handled the event.
            let routed = match &event {
                DaemonEvent::HidKeyEvent { keycode } => {
                    wrapper::route_hid_key(&state_for_events, *keycode).await
                }
                DaemonEvent::HidTypeString { text, send_enter } => {
                    wrapper::route_hid_type_string(&state_for_events, text, *send_enter).await
                }
                _ => false,
            };
            if routed {
                continue;
            }

            // Forward to WS client
            ws::forward_event_to_ws(&state_for_events, &event).await;
        }
    });

    // Wait for shutdown
    tokio::select! {
        _ = server => {}
        _ = event_handler => {}
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down...");
        }
    }
}

/// Run the winit event loop on the main thread (for tray icon support on macOS)
fn run_main_event_loop(
    state: Arc<DaemonState>,
    tray_manager: Option<tray::DaemonTrayManager>,
    tray_action_rx: Option<std::sync::mpsc::Receiver<tray::DaemonTrayAction>>,
    tray_update_rx: std::sync::mpsc::Receiver<TrayUpdate>,
) {
    use winit::application::ApplicationHandler;
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};

    struct TrayApp {
        state: Arc<DaemonState>,
        tray_manager: Option<tray::DaemonTrayManager>,
        tray_action_rx: Option<std::sync::mpsc::Receiver<tray::DaemonTrayAction>>,
        tray_update_rx: std::sync::mpsc::Receiver<TrayUpdate>,
    }

    impl ApplicationHandler for TrayApp {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            // WaitUntil lets us drain the mpsc channels (tray_update_rx,
            // tray_action_rx) on a regular tick. Pure Wait would only fire
            // about_to_wait on native UI events, which leaves async-side
            // updates (hooks installed, wrapper connected) sitting in the
            // channel until the user happens to open the menu.
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                std::time::Instant::now() + std::time::Duration::from_millis(250),
            ));
        }

        fn window_event(
            &mut self,
            _event_loop: &ActiveEventLoop,
            _window_id: winit::window::WindowId,
            _event: winit::event::WindowEvent,
        ) {}

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            // Re-arm the periodic tick so the channels keep draining even
            // when no native UI events arrive.
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                std::time::Instant::now() + std::time::Duration::from_millis(250),
            ));

            // Process tray updates from async code (non-blocking)
            while let Ok(update) = self.tray_update_rx.try_recv() {
                if let Some(ref mut tray) = self.tray_manager {
                    match update {
                        TrayUpdate::DeviceConnected { name, firmware } => {
                            tray.set_device_status(
                                tray::DevicePresence::Active,
                                Some(&name),
                                Some(&firmware),
                            );
                        }
                        TrayUpdate::DeviceDisconnected => {
                            // Device interface closed but may still be physically available
                            tray.set_device_status(tray::DevicePresence::Available, None, None);
                        }
                        TrayUpdate::DeviceAvailable(name) => {
                            tray.set_device_status(
                                tray::DevicePresence::Available,
                                Some(&name),
                                None,
                            );
                        }
                        TrayUpdate::DeviceUnavailable => {
                            tray.set_device_status(tray::DevicePresence::None, None, None);
                        }
                        TrayUpdate::Tabs(list) => {
                            tray.set_tabs(&list);
                        }
                        TrayUpdate::HooksInstalled(installed) => {
                            tray.set_hooks_installed(installed);
                        }
                    }
                }
            }

            // Poll tray menu actions (non-blocking)
            if let Some(ref rx) = self.tray_action_rx {
                while let Ok(action) = rx.try_recv() {
                    match action {
                        tray::DaemonTrayAction::FocusWrapper(wrapper_id) => {
                            let state = Arc::clone(&self.state);
                            std::thread::spawn(move || {
                                let rt = tokio::runtime::Builder::new_current_thread()
                                    .enable_all()
                                    .build()
                                    .unwrap();
                                rt.block_on(async {
                                    if let Err(e) = wrapper::set_active_wrapper(&state, &wrapper_id).await {
                                        info!("FocusWrapper({}) failed: {}", wrapper_id, e);
                                    }
                                });
                            });
                        }
                        tray::DaemonTrayAction::OpenSettings => {
                            let url = format!(
                                "http://{}/settings",
                                self.state.listen_addr,
                            );
                            info!(url = %url, "opening settings page");
                            open_url_in_browser(&url);
                        }
                        tray::DaemonTrayAction::InstallHooks => {
                            let listen = self.state.listen_addr.clone();
                            let state = Arc::clone(&self.state);
                            std::thread::spawn(move || {
                                hooks::install_claude_hooks(&listen);
                                let installed = hooks::are_hooks_installed();
                                state.send_tray_update(TrayUpdate::HooksInstalled(installed));
                            });
                        }
                        tray::DaemonTrayAction::Quit => {
                            info!("Quit requested from tray");
                            event_loop.exit();
                        }
                    }
                }
            }
        }
    }

    let event_loop = EventLoop::new().expect("Failed to create event loop");
    let mut app = TrayApp {
        state,
        tray_manager,
        tray_action_rx,
        tray_update_rx,
    };
    let _ = event_loop.run_app(&mut app);

    info!("Daemon exiting");
    std::process::exit(0);
}

#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn setup_macos_accessory() {
    use cocoa::appkit::NSApp;
    use objc::{sel, sel_impl};

    unsafe {
        let app = NSApp();
        // NSApplicationActivationPolicyAccessory = 1 (no dock icon)
        let _: () = objc::msg_send![app, setActivationPolicy: 1_isize];
    }
}

#[cfg(not(target_os = "macos"))]
fn setup_macos_accessory() {}

/// Spawn a platform-appropriate "open URL in default browser" command.
/// Best-effort and non-blocking — tray actions shouldn't be load-bearing.
fn open_url_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let (program, args): (&str, Vec<&str>) = ("open", vec![url]);
    #[cfg(target_os = "linux")]
    let (program, args): (&str, Vec<&str>) = ("xdg-open", vec![url]);
    #[cfg(target_os = "windows")]
    let (program, args): (&str, Vec<&str>) = ("cmd", vec!["/C", "start", "", url]);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        info!(url = %url, "open_url_in_browser: unsupported platform");
        return;
    }

    if let Err(e) = std::process::Command::new(program).args(&args).spawn() {
        info!(error = %e, program, "open_url_in_browser failed to spawn");
    }
}

// ── Signal handling ────────────────────────────────────────────────

/// Install a raw SIGINT/SIGTERM handler that terminates the process.
///
/// On macOS, winit runs NSApplication::run() which installs its own signal
/// handlers that swallow SIGINT. tokio::signal::ctrl_c() never fires because
/// the signal is consumed before kqueue sees it. This handler runs before
/// winit and ensures Ctrl-C actually exits.
fn install_signal_handler() {
    extern "C" fn handle_signal(_sig: libc::c_int) {
        // Use _exit to avoid running atexit handlers which could deadlock
        unsafe { libc::_exit(0) }
    }
    unsafe {
        libc::signal(libc::SIGINT, handle_signal as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handle_signal as libc::sighandler_t);
    }
}

// ── launchd install/uninstall ──────────────────────────────────────

fn install_launchd(listen: &str) {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").expect("HOME not set");
        let plist_dir = format!("{}/Library/LaunchAgents", home);
        let plist_path = format!("{}/com.coredeck.daemon.plist", plist_dir);

        let exe = std::env::current_exe()
            .expect("Failed to get current exe path")
            .to_string_lossy()
            .to_string();

        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.coredeck.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>--listen</string>
        <string>{listen}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{home}/Library/Logs/coredeck.log</string>
    <key>StandardErrorPath</key>
    <string>{home}/Library/Logs/coredeck.log</string>
</dict>
</plist>"#
        );

        std::fs::create_dir_all(&plist_dir).expect("Failed to create LaunchAgents dir");
        std::fs::write(&plist_path, plist).expect("Failed to write plist");

        // Idempotent reload: unload silently first (no-op if not loaded), then
        // load the (possibly updated) plist. Using `bootout`+`bootstrap` would
        // be cleaner on modern macOS but requires the per-uid domain spelled
        // out, and `load`/`unload` still works.
        let _ = std::process::Command::new("launchctl")
            .args(["unload", &plist_path])
            .stderr(std::process::Stdio::null())
            .status();

        let status = std::process::Command::new("launchctl")
            .args(["load", &plist_path])
            .status()
            .expect("Failed to run launchctl");

        if status.success() {
            println!("Installed and loaded: {}", plist_path);
        } else {
            eprintln!("launchctl load failed (exit {})", status.code().unwrap_or(-1));
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = listen;
        eprintln!("launchd is only available on macOS");
    }
}

/// One-shot setup: install Claude Code hooks, register launchd, and tell
/// the user how to alias `claude` to the wrapper. Idempotent — safe to
/// re-run after upgrading.
fn run_setup(listen: &str) {
    println!("=== CoreDeck setup ===\n");

    println!("1/2 Installing Claude Code hooks…");
    hooks::install_claude_hooks(listen);

    println!("\n2/2 Registering launchd auto-start…");
    install_launchd(listen);

    println!();
    println!("Done. To finish, alias `claude` to the wrapper in your shell rc:");
    println!();
    println!("  # ~/.zshrc (or ~/.bashrc)");
    println!("  alias claude=\"coredeck-claude\"");
    println!();
    println!("Then `claude` in any terminal will run under CoreDeck.");
}

fn uninstall_launchd() {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").expect("HOME not set");
        let plist_path = format!("{}/Library/LaunchAgents/com.coredeck.daemon.plist", home);

        if std::path::Path::new(&plist_path).exists() {
            let _ = std::process::Command::new("launchctl")
                .args(["unload", &plist_path])
                .status();
            std::fs::remove_file(&plist_path).expect("Failed to remove plist");
            println!("Uninstalled: {}", plist_path);
        } else {
            println!("Plist not found: {}", plist_path);
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("launchd is only available on macOS");
    }
}

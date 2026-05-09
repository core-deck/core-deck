# CoreDeck Roadmap

CoreDeck is a daemon-only architecture: one background daemon
(`coredeck`) owns the HID device, the tray icon, the HTTP+WS APIs,
and Claude Code hook endpoints. A thin wrapper binary
(`coredeck-claude`) runs `claude` under a PTY in any host terminal
(Terminal.app, iTerm2, Ghostty, WezTerm, Kitty, tmux, GNOME Terminal,
Konsole, Alacritty, JetBrains-family embedded terminals, …),
registers with the daemon over WebSocket, and accepts byte-injection
commands. There is no GUI app — soft-key editing and other
configuration live on a static settings page served by the daemon at
`http://127.0.0.1:19384/` and opened in the user's default browser.

This document records open backlog, recently-shipped features, and a
few architectural notes worth keeping handy.

## Architecture

```
coredeck-protocol     ← wire types (serde only)
coredeck              ← tray + HTTP/WS server + Claude Code hook endpoints + static settings page
coredeck-claude       ← thin PTY wrapper, registers with daemon
```

The daemon hosts every user-facing surface:

- **Tray menu** — live tab list (per-wrapper rows with session name +
  current task), click a row to set active and raise the host terminal,
  device info, "Open Settings…", "Quit Daemon".
- **HTTP-served settings page** at `/` and `/settings` — HTML/CSS/JS
  embedded in the daemon binary, opened in the user's default browser
  via the tray menu. Hosts the soft-key editor.
- **Hook-driven session state** — per-session metadata gathered from
  Claude Code's HTTP hooks (PreToolUse, PostToolUse, Stop, Notification,
  PermissionRequest, UserPromptSubmit, SessionStart, SessionEnd,
  PreCompact, statusLine, subagentStatusLine), keyed by `session_id`,
  bound to wrappers via the `COREDECK_WRAPPER_ID` env var that the
  SessionStart command hook reads.

## Done (recent highlights)

- **Daemon-only architecture.** Old GUI crate and its
  egui/wezterm/glutin/eframe dep tree deleted; the daemon
  (`crates/coredeck`) is the sole executable referenced by
  `CFBundleExecutable`. The wrapper (`coredeck-claude`) ships
  alongside it inside `Contents/MacOS/`.
- **`coredeck-claude` wrapper.** Thin PTY around `claude` in any host
  terminal, registers with the daemon over `/wrapper-ws`, accepts
  byte-injection commands. Smoke-tested in Terminal.app and iTerm2.
- **Tray menu live tab list.** One row per wrapper with session name +
  task subtitle (native `NSMenuItem.subtitle` on macOS 14.4+), active
  row marked via NSMenuItem state column for clean alignment, click
  to set active.
- **Hook coverage.** Daemon handles PreToolUse, PostToolUse,
  PermissionRequest, Stop, Notification, UserPromptSubmit,
  SessionStart, SessionEnd, PreCompact, statusLine, subagentStatusLine.
  Each hook routes by `session_id` into `SessionState`.
- **Mode-toggle and soft-keys via wrapper.** Daemon injects bytes into
  the active wrapper's PTY via `DaemonToWrapper::Write`. Mode-LED
  reflects active session's `permission_mode` and the mode-button tap
  cycles regardless of focus (`\x1b[Z` injection).
- **Interactive PermissionRequest routing.** Non-YOLO requests park
  the HTTP response in a `oneshot`, ship the prompt to the device,
  resolve on Allow/Deny via `Enter`/`y` or `Esc`/`n`/`Ctrl-C`. Parallel
  pending alerts are queued (depth-bounded `VecDeque`); a queued alert
  surfaces as soon as the current one resolves.
- **OSC 1004 focus detection.** Wrapper enables focus reporting at
  startup, parses `ESC [ I` / `ESC [ O` from stdin, forwards
  `FocusChanged`. Daemon updates `active_wrapper_id` accordingly.
- **F20 raises the wrapper's terminal.** Per-host adapter detected
  from env at startup (Terminal.app via AppleScript by tty, iTerm2 by
  session id, WezTerm/Kitty/tmux via their CLIs). Clears any idle
  alert tied to that session as a side effect.
- **subagentStatusLine integration.** `WrapperTab.subagent_label` /
  `subagent_count` populated from the panel; device line 1 shows the
  in-flight subagent label with `(N)` count when more than one runs.
- **Better session titles.** Priority chain: `session_name` (`/rename`
  or `--name`) → OSC 0/1/2 terminal title (sniffed by the wrapper
  from claude's PTY output, with a leading-glyph and trailing
  ` Claude Code` strip) → right-truncated cwd. Tray menu uses a
  wider, segment-aligned cwd cap.
- **New wrapper steals device focus on SessionStart.** Was a known
  bug: opening a fresh `claude` in an already-focused terminal didn't
  promote it on the device because the SessionStart bind only ran
  `set_active_session` when nothing was active, and OSC 1004 focus-in
  doesn't fire when there's no focus *transition* (the terminal was
  already focused). Now every SessionStart promotes — running `claude`
  requires typing into a focused terminal, so SessionStart itself is
  the "user is here" signal. The accepted corner case is a wrapper
  auto-spawned in a background pane (e.g. `tmux split-window claude`),
  which will incorrectly steal focus on the device until the user
  knob-cycles back. Compaction/resume forks promote correctly too —
  the new session_id is the live one.
- **Per-wrapper Auto-approve enrollment.** Global Auto-approve toggle
  is still a single hardware switch, but it's gated on a per-wrapper
  enrollment state — three values per wrapper: opted-in, opted-out,
  or undecided. The wrapper focused at the moment the toggle flips on
  is auto-opted-in; every undecided wrapper sees an "Auto-approve this
  tab?" alert on its first PermissionRequest (no tool name — enrollment
  covers everything). Allow enrolls (and approves this PR plus every
  future one in that wrapper). Deny records opt-out so the daemon
  doesn't re-prompt; subsequent PRs fall straight through to Claude's
  terminal until Auto-approve toggles, the device disconnects, or the
  wrapper exits. Refinements landed since the original drop: a single
  Allow tap auto-resolves every sibling parallel-tool PR (the queue
  re-checks enrollment on each pop, so N parallel Bash calls clear
  with one tap instead of N), and `PreToolUse`/`PostToolUse` only
  cancel the active alert when their `tool_name` matches — sibling
  parallel hooks no longer evict each other.
- **Tools that wait on the user skip auto-approve.** `ExitPlanMode`
  was already excluded; `AskUserQuestion` joined it after we noticed
  the daemon was happily approving questions with no answer attached
  (Claude resumed with an empty result). Generalised to a
  `user_input_tool` predicate so the next tool of this shape is one
  match-arm away.
- **PermissionRequest gets a long timeout.** The shim was running
  `curl -m 5` for every hook; fine for observational ones, fatal for
  PRs since the user's reaction time is naturally longer. Now the
  shim takes an optional `max-time` argument (default 5s) and the
  PermissionRequest entry passes `1800` plus a matching `timeout: 1800`
  on the hook entry so Claude's own 60s default doesn't bail first.
  The daemon's internal 5-min timeout remains the outer limit.
- **Robustness.** Wrapper has bounded-exponential WS reconnect
  backoff (1s→30s); daemon preserves prior `session_id` across
  re-register. Cross-restart session caching: the daemon pushes a
  `SessionBound` message to the wrapper after binding, the wrapper
  caches the value, and a subsequent Register echoes it back so a
  daemon restart restores the session→wrapper binding without
  waiting for the user's next prompt. YOLO gates on
  `device_status.connected && yolo` so a disconnected device can't
  auto-approve. `hooks install` is non-destructive (deep-merge),
  and a curl shim (`~/.claude/coredeck-hook.sh`) swallows
  ECONNREFUSED so claude doesn't error when the daemon is down.
- **Static settings page** at `/` and `/settings` — embedded HTML/JS,
  hosts the soft-key editor; tray "Open Settings…" launches the
  default browser. Hooks install/uninstall is also exposed via
  `/api/hooks/*`.
- **Daemon renamed `coredeck-daemon` → `coredeck`.** With no GUI app
  the `-daemon` suffix was redundant. Crate dir, bin, CLI command,
  log path (`~/Library/Logs/coredeck.log`), `CFBundleExecutable`,
  and CI workflows updated. launchd label `com.coredeck.daemon` is
  unchanged (it's the role descriptor, not the binary name).
- **Signed/notarized macOS bundle.** Both binaries (daemon +
  wrapper) ship inside `Contents/MacOS/` and are signed inside-out
  with the hardened runtime. `LSUIElement` is set so the bundle is
  truly tray-only (no Dock icon). Entitlements were tightened —
  the cs.* relaxations (allow-jit, allow-unsigned-executable-memory,
  disable-library-validation) were removed; a pure Rust daemon
  doesn't need them and they only weaken hardened runtime.
- **Installer ergonomics.** `brew install --cask` ships the .app and
  symlinks both binaries onto PATH; `coredeck setup` chains
  `hooks install` + idempotent launchd registration in one shot, then
  prints the `alias claude=coredeck-claude` snippet (we deliberately
  don't auto-edit the user's shell rc). The cask uninstall stops the
  launchd agent and `--zap` clears hook config + logs. For users who
  install the .app from a DMG without brew, the tray menu surfaces an
  "⚠ Install Claude Code hooks…" item until hooks are present.
- **Remote claude over SSH.** `coredeck-claude --ssh user@host` opens
  an interactive remote shell with `COREDECK_WRAPPER_ID` and
  `COREDECK_DAEMON_URL` exported and an `ssh -R` reverse tunnel
  pointing at the local daemon. The user runs claude (or `tmux new`
  then claude) from there; hooks fire back through the tunnel,
  session correlation works the same as local. Pair with
  `coredeck setup --remote user@host` (one-shot SSH-based hook
  install on the remote box). Tunnel mirrors the local daemon port
  (default 19384) so claude's baked-in URL stays valid across
  reconnects. tmux env propagation handled via `tmux setenv -g/-t`
  on connect — new panes in already-running tmux see the fresh env.
  Remote tabs are marked with a leading `↦` (U+21A6, in Terminus'
  coverage) on both the tray menu and the device's session label so
  they're easy to spot. Trust boundary is SSH itself; no tokens, no
  TLS. Tradeoff: one wrapper per remote host at a time (port
  collision is a clean fast-fail thanks to `ExitOnForwardFailure=yes`).
- **Persistent wrapper_id per --ssh host.** Each `coredeck-claude
  --ssh` invocation reads/writes `<data_dir>/wrappers/<host>` for a
  stable UUID, so claude processes alive in remote tmux keep
  correlating to the same wrapper across reconnect cycles.
- **F20+Stop opens a fresh claude in the focused cwd.** Firmware
  (in the sibling firmware repo) tracks the Claude button's held
  state and emits `KC_F24` when Stop is tapped while it's down —
  `Ctrl-C` is unchanged for plain Stop. The daemon's new `spawn`
  module mirrors `raise.rs` with per-host adapters
  (Terminal.app/iTerm2 via AppleScript, WezTerm/Kitty/tmux via
  their CLIs, Ghostty via `open -na`); local sessions only for v1
  — `--ssh` falls back to a debug log. The wrapper binary path is
  derived from `current_exe()`'s sibling so dev builds running out
  of `target/debug/` spawn the same binary that started the daemon
  rather than whatever's on `PATH`.
- **JetBrains terminal support** (IntelliJ, Android Studio, PyCharm,
  GoLand, …). Wrapper detects the embedded JediTerm via
  `$TERMINAL_EMULATOR=JetBrains-JediTerm` and stuffs
  `$__CFBundleIdentifier` into `pane_id` so the daemon knows which
  IDE to bring forward (Android Studio vs IntelliJ vs …). JediTerm
  explicitly stubs OSC 1004 in `JediEmulator.java` — no focus
  reports — so the daemon (1) suppresses idle alerts for these
  sessions (no way for the user to clear them via focus) via a
  `supports_focus_reporting()` predicate on `HostTerminalKind`,
  (2) raises by bundle id on F20 (`tell application id "<bundle>"
  to activate`), and (3) polls `NSWorkspace.frontmostApplication`
  every 500 ms to promote the matching JetBrains wrapper when the
  user Cmd-Tabs into the IDE. Polling is short-circuited when no
  JetBrains wrappers are connected. No new entitlements — these are
  unprivileged AppKit APIs. Caveat: two project windows of the same
  IDE share a bundle id, so the watcher picks the first match.
- **Idle alert quality of life.** Tracked `is_focused` on each
  Wrapper from the OSC 1004 frames the wrapper already sent
  (focus-out was previously dropped); `show_idle_alert`
  short-circuits when the alerting session's terminal is currently
  focused — claude's in-terminal prompt is enough, the device
  alert was just noise. Combined with the JetBrains
  `supports_focus_reporting()` skip, idle alerts only surface when
  they can actually help.
- **Tray active-row indicator.** Replaced the system ✓ with a
  filled circle drawn into a custom NSImage. Two variants are built
  per render: a 14×32 top-biased canvas for two-line rows (so the
  dot lands at the title's vertical level rather than between title
  and subtitle) and a 14×14 centered canvas for single-line rows
  (so the dot stays mid-row). `labelColor` keeps light/dark and
  accessibility contrast working.
- **Device task2 enrichment.** `TaskUpdate(status=…)` events used
  to render as a bare "TaskUpdate" on line 2 because the payload
  carries only `task_id` + `status`. Daemon now joins the cached
  subject from `TaskCreated` with a status-specific glyph
  (`✓ Subject` for completed, `✗ Subject` for cancelled/failed,
  `○ Subject` otherwise). The `in_progress` branch is unchanged —
  it still pins the subject to line 1 via `current_task`.
  `AskUserQuestion` clears line 2 instead of writing
  "AskUserQuestion: …" so the Idle alert overlay owns the screen.
- **Linux daemon parity.** Workspace builds cleanly on Ubuntu
  (apt: `libudev-dev`, `libhidapi-dev`). `coredeck install` writes
  a systemd user unit at `~/.config/systemd/user/coredeck.service`
  (no root, journald handles logs) and runs `daemon-reload`+
  `enable --now`; `coredeck uninstall` reverses it. `coredeck setup`
  branches on platform — launchd on macOS, systemd on Linux. Spawn
  adapters cover GNOME Terminal (`--working-directory= -- cmd`),
  Konsole (`--new-tab --workdir`), and Alacritty
  (`--working-directory -e`); WezTerm/Kitty/tmux already worked via
  their cross-platform CLIs. Raise uses `$WINDOWID` via
  `wmctrl -ia` when the terminal sets it, falling back to
  `wmctrl -x -a <class>` keyed off `WM_CLASS`. Wrapper detects
  Linux terminals via `$KONSOLE_VERSION`, `$ALACRITTY_LOG`, and
  `$GNOME_TERMINAL_SCREEN`/`$VTE_VERSION`. HID hot-plug is
  event-driven via libudev (`MonitorBuilder.match_subsystem_devtype
  ("usb", "usb_device")`) so plug/unplug doesn't wait for the 2s
  poll tick — same UX as the macOS IOKit watcher.
- **Update checker.** Daemon polls
  `https://api.github.com/repos/core-deck/{core-deck,firmware}/releases/latest`
  once on startup (after a 60s grace window) and every 24h
  thereafter. Newer-than-current tags surface as "Update available:
  daemon vX.Y.Z" / "firmware vX.Y.Z" rows in the tray menu, sitting
  just above the "Install hooks…" / Settings rows. Click opens the
  release page in the user's default browser. No autoupdate, no
  signature verification — Homebrew handles macOS upgrades, the
  user reflashes the device by hand. Firmware row only appears
  once the device has reported a parseable version. No opt-out
  toggle yet (add when someone asks).

---

## Open backlog

- **JetBrains: disambiguate multi-window same-IDE.** Two IntelliJ
  project windows share a bundle id, so the frontmost-app watcher
  promotes whichever wrapper it finds first. Process-tree walk
  (parent-PID up to the IDE process, match wrapper PIDs to specific
  windows) would close this. Low priority — the common case is one
  wrapper per IDE.
- **F20+Stop on remote (`--ssh`) sessions.** The chord currently
  short-circuits with a debug log. A proper remote-spawn flow
  (open a new tmux window or a new ssh-tunneled wrapper, depending
  on what the user actually wants) is its own slice.
- **Linux frontmost-app watcher.** macOS uses
  `NSWorkspace.frontmostApplication` to follow Cmd-Tab into
  JetBrains IDEs (and any other terminal whose embedded session
  doesn't emit OSC 1004). The X11 equivalent is polling
  `_NET_ACTIVE_WINDOW` + reading `WM_CLASS`; on pure Wayland this
  is per-DE (GNOME Shell extensions, KDE D-Bus, Sway IPC) with no
  cross-DE primitive. Out of scope until a user actually hits it.
- **Linux JetBrains raise.** Bundle-id raise relies on the macOS
  `__CFBundleIdentifier` env var. Linux JetBrains apps don't set
  it; would need to capture the IDE's `WM_CLASS` instead and raise
  via `wmctrl -x -a <class>` (we already have the helper). Trivial
  but unverified; needs a Linux box with an IDE installed to test.
- **Windows.** PTY + raw mode work via `portable-pty` + `crossterm`
  in theory but ConPTY hasn't been validated. No raise/spawn
  adapters yet (`SetForegroundWindow` + each terminal's CLI would
  cover most). HID device access works through the same `hidapi`
  crate but service registration would need `sc create` or a
  Win32 Service wrapper instead of launchd/systemd.
- **Intel macOS.** Dropped from the release workflow when GitHub's
  `macos-13` runner queue started blocking tagged builds for 12+
  hours. The code still builds for `x86_64-apple-darwin` from
  source — just not in CI. Re-add a `build-macos-x86_64` job + the
  lipo step in `release.yml` (it's all kept in `bundle.sh` behind
  `--lipo-only`) when GitHub's Intel availability stabilises or
  if anyone actually asks.

---

## Architectural notes worth keeping

- **Correlation strategy**: `coredeck-claude` sets
  `COREDECK_WRAPPER_ID` in the child's env. The SessionStart command
  hook script (`coredeck-register.sh`) inherits that env, reads
  `session_id` from its stdin payload, and POSTs `{wrapper_id,
  session_id}` to the daemon. The daemon never has to guess.
  Robust against parallel wrappers in the same directory and against
  forks (each compaction fork triggers a new SessionStart with the
  same wrapper_id but a new session_id, so the wrapper's session_id
  follows automatically).

- **Hooks responses**: `PermissionRequest` is the only hook that
  expects a response with semantic meaning (allow/deny). All others
  return 200 OK and are observational. Keep that distinction in mind
  when adding handlers.

- **Wrapper's PTY ownership**: the wrapper owns the PTY master, the
  user's terminal owns its own tty. Keystrokes injected by daemon
  reach claude through the wrapper's `Write` message and go via the
  master. The user's terminal doesn't see them unless claude echoes
  back.

- **Display authority**: the daemon is the sole display authority —
  it projects `WrapperTabList` to `DisplayUpdate` in
  `wrapper.rs::push_to_device`. External WS clients can still send
  `UpdateDisplay` over `/ws`, but they're cooperating with the
  daemon, not replacing it.

- **Alerts replaced by hooks**: `Notification(permission_prompt)` and
  `PermissionRequest` carry the prompts we care about; the wrapper
  doesn't need an OSC parser for alerts. The wrapper does sniff
  OSC 0/1/2 for terminal titles (session-label fallback) and parses
  OSC 1004 focus reports — but those are passive observations, not
  alert sources.

- **Two WS endpoints**: `/ws` is the public client API (binary
  framed, exclusive lock). `/wrapper-ws` is the wrapper protocol
  (JSON framed, no lock — many wrappers connect concurrently). Don't
  conflate the two when adding new messages.

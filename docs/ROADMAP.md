# CoreDeck Roadmap

CoreDeck is a daemon-only architecture: one background daemon
(`coredeck-daemon`) owns the HID device, the tray icon, the HTTP+WS
APIs, and Claude Code hook endpoints. A thin wrapper binary
(`coredeck-claude`) runs `claude` under a PTY in any host terminal
(Terminal.app, iTerm2, Ghostty, Kitty, tmux, …), registers with the
daemon over WebSocket, and accepts byte-injection commands. There is
no GUI app — soft-key editing and other configuration live on a
static settings page served by the daemon at `http://127.0.0.1:19384/`
and opened in the user's default browser.

This document records open backlog, recently-shipped features, and a
few architectural notes worth keeping handy.

## Architecture

```
coredeck-protocol     ← wire types (serde only)
coredeck-daemon       ← tray + HTTP/WS server + Claude Code hook endpoints + static settings page
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

- **Daemon-only architecture.** `crates/coredeck/` GUI crate and its
  egui/wezterm/glutin/eframe dep tree deleted; macOS app bundles
  `coredeck-daemon` as the sole executable.
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
  wrapper exits.
- **Robustness.** Wrapper has bounded-exponential WS reconnect
  backoff (1s→30s); daemon preserves prior `session_id` across
  re-register. YOLO gates on `device_status.connected && yolo` so a
  disconnected device can't auto-approve. `hooks install` is
  non-destructive (deep-merge), and a curl shim
  (`~/.claude/coredeck-hook.sh`) swallows ECONNREFUSED so claude
  doesn't error when the daemon is down.
- **Static settings page** at `/` and `/settings` — embedded HTML/JS,
  hosts the soft-key editor; tray "Open Settings…" launches the
  default browser. Hooks install/uninstall is also exposed via
  `/api/hooks/*`.

---

## Open backlog

- **Active-tab not switching when a new wrapper opens focused.**
  Opening a new claude session in a freshly-focused terminal
  sometimes leaves the device pointing at the previously-active
  wrapper. Suspected race between Register / SessionStart / OSC 1004
  focus-in. Repro is intermittent — capture the daemon log next time
  and look for `set_active_wrapper from focus-in failed` or a late
  FocusOut around the new wrapper's Register.
- **Statusline extras.** Currently used: `context_window`, `cost`,
  `model`, `session_name`, `effort.level`, `thinking.enabled`. Available
  but ignored: `rate_limits.{five_hour,seven_day}`, `agent.name`,
  `worktree.*`, `exceeds_200k_tokens`. Best fit is non-task surfaces —
  tray menu rows (rate-limit countdown), an alert when
  `exceeds_200k_tokens` flips, the settings page for
  worktree/agent metadata.
- **Third source for session titles.** `session_name` and OSC 0/1/2
  terminal title are wired; `worktree.branch` from statusline could
  fill in for unnamed wrappers. Manual rename via tray is also open.
- **Cross-restart session caching.** A daemon restart wipes the
  in-memory wrapper map, so cross-restart session correlation needs
  the next SessionStart hook to rebind. Possible fix: cache
  `session_id` in the wrapper itself and re-send it in Register.
- **Resume mapping.** When a user runs `claude --resume <id>` inside
  a wrapper, the wrapper learns the resumed session_id lazily via
  SessionStart. A small race window exists; could be closed by
  passing `--resume <id>` through to the wrapper and pre-binding.
- **Optional rename: `coredeck-daemon` → `coredeck`.** With no GUI
  app the `-daemon` suffix is redundant. Cosmetic; would need to
  update launchd plist, bundle scripts, install docs.
- **Remote daemon.** Wrapper already supports `COREDECK_DAEMON_ADDR`,
  so running claude on a remote dev box with local CoreDeck is
  theoretically possible. Not tested; would need TLS for anything
  serious.
- **Linux / Windows.** PTY + raw mode + SIGWINCH work on Unix via
  `portable-pty` + `crossterm`. Windows needs ConPTY validation;
  Linux should be fine but unverified.
- **Installer ergonomics.** `brew install` + a `coredeck setup`
  command that installs the daemon, registers launchd, runs
  `hooks install`, sets up the `claude` alias. Today all manual.
- **Multiple wrappers in same cwd.** Rare but possible. Today
  distinguished only by `wrapper_id` (env-var correlation — `cwd`
  was an earlier fallback that's no longer needed).

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

# CoreDeck Companion-Mode Roadmap

CoreDeck is moving from "embedded terminal app + daemon" to "thin
wrapper + daemon, no GUI app." The user runs `coredeck-claude` in their
own terminal (iTerm2, Ghostty, Kitty, tmux, …); the wrapper owns a PTY
around `claude`, registers with the daemon over WebSocket, and accepts
byte-injection commands. The daemon owns hook-driven per-session state,
drives the device display, and (eventually) hosts every user-facing
surface — tray menu for live status, browser-served settings page for
configuration.

This document records what's shipped, what's discussed but not yet built,
and what's parked for later.

## Why this exists

The old architecture (embedded wezterm + egui terminal) was the only way
to: detect OSC alerts, track session focus, set permission mode from
hardware, render tabs, route HID input to the right Claude. Claude Code
hooks have closed enough of those gaps that we can sunset the embedded
terminal — but only if we keep the killer property: **control Claude
from the device without screen-switching**. The wrapper exists to
preserve that property in any host terminal.

The trade-off the user accepted: type `coredeck-claude` (or alias
`claude`) instead of `claude`. In exchange we get to delete the entire
`crates/coredeck/` crate — renderer, PTY, OSC parser, fork-detection,
sessions, tabs, themes, bookmarks, plus the egui/wezterm/glutin/eframe
dep tree. Soft-key editing moves to a static page served by the daemon
and opened in the user's default browser. Live tab status moves into
the daemon's tray menu. The final shape is two binaries:
`coredeck-daemon` and `coredeck-claude`.

---

## Now shipping (built and smoke-tested)

| Component | Path | Status |
|---|---|---|
| `coredeck-claude` wrapper binary | `crates/coredeck-claude/` | working |
| Wire protocol additions | `crates/coredeck-protocol/src/lib.rs` | `Register`, `Goodbye`, `Registered`, `Write`, `WrapperRegisterSession`, `WrapperTab`, `WrapperTabList`, `WrapperWriteRequest`, `SetActiveWrapperRequest` |
| Daemon `/wrapper-ws` (multi-connection) | `crates/coredeck-daemon/src/wrapper.rs` | working |
| Daemon `POST /wrapper/register` | same | working |
| `coredeck-register.sh` SessionStart hook | `crates/coredeck-daemon/scripts/coredeck-register.sh` (embedded via `include_str!`) | working |
| `hooks install` writes script + adds SessionStart | `crates/coredeck-daemon/src/hooks.rs` | working |
| Per-session `ClaudeState` (`SessionState` keyed by `session_id`) | `crates/coredeck-daemon/src/state.rs` | working |
| Hook handlers route by `session_id` | `crates/coredeck-daemon/src/hooks.rs` | working |
| `WrapperTabList` event over `/ws` | emitted on every state change | working |
| WS commands `WrapperWrite`, `SetActiveWrapper` | `crates/coredeck-daemon/src/ws.rs` | working |
| Daemon projects tab list directly to HID display | `crates/coredeck-daemon/src/wrapper.rs::push_to_device` | working — daemon now drives the device when wrappers are present |
| Embedded GUI deleted (`crates/coredeck/`, wezterm/egui dep tree, `patches/zune-jpeg/`) | n/a | done — see step 9 |

End-to-end verified: wrapper connect → register → session correlation
via curl AND via running `coredeck-register.sh` with `$COREDECK_WRAPPER_ID`
+ synthetic SessionStart payload → wrapper disconnect cleanup. Device
display updates with session label, tab indicators, model/cost/context
on every hook + statusline event.

The embedded terminal still runs in parallel; nothing has been deleted
yet. Switching to companion mode is opt-in by running `coredeck-claude`.

---

## Immediate next: dogfood the wrapper

Before building more on top, validate that the trade-off feels right
with the device in hand. Concretely:

1. `alias claude='/Users/vden/work/agentdeck/app/target/release/coredeck-claude'`
2. Run `coredeck-daemon hooks install` once to refresh settings.json
   with the new `SessionStart` entry. (The current production daemon
   may also need to be rebuilt to handle `SessionStart` HTTP hooks
   without 404; either way the command hook works regardless.)
3. Open one or two terminals, start `claude` in each. Confirm:
   - `WrapperTabList` arrives in the app's debug log on connect / hook
     / disconnect.
   - `wrapper bound to session …` appears in daemon log on `SessionStart`.
4. Live with it for a few days. Note where the device-side experience
   feels worse than the embedded terminal — that's the priority list
   for the next pass.

If something is broken, things to look at first:
- `~/.claude/settings.json` — confirm the `SessionStart` entry has both
  `command` (the script) and `http` (daemon) entries.
- `~/.claude/coredeck-register.sh` — confirm it's executable.
- `COREDECK_LOG=debug coredeck-claude` for wrapper-side WS traffic.

---

## Target end state: daemon-only, no GUI app

Once the wrapper handles the terminal experience, the GUI app loses
nearly all of its purpose. The clean endpoint is:

```
coredeck-protocol     ← wire types
coredeck-daemon       ← tray + HTTP/WS server + static settings page
coredeck-claude       ← thin PTY wrapper
~~crates/coredeck/~~  ← deleted entirely
```

The daemon hosts every user-facing surface:

- **Tray menu** — live tab list (active sessions with model + context %),
  click a row to set active + raise the host terminal, brightness submenu,
  YOLO toggle, hooks install/uninstall, "Open Settings…", Quit.
- **HTTP-served settings page** at `http://127.0.0.1:19384/settings` —
  HTML/CSS/JS, opened in the user's default browser via the tray menu.
  Hosts the soft-key editor and any settings that don't fit in the tray.
- **No native GUI app, no egui, no wezterm** — `crates/coredeck/` and
  all of its rendering deps are gone.

Trade-offs accepted:
- Soft-key editor moves from native egui to browser HTML/JS. Adequate;
  the editor is the only meaningful UI and it's not interaction-heavy.
- Loses some macOS menu-bar polish (the daemon's tray is enough).
- One more fixed cost up front (a JS soft-key UI), in exchange for
  deleting the entire `coredeck` crate, six wezterm git deps, and
  egui/glutin/eframe.

## Next steps (priority order, each phase dogfoodable)

### 1. Tray menu shows the live tab list — DONE

`tray.rs` rebuilds the menu on every `WrapperTabList` change: one row
per wrapper with its session label, click to set active. Empty-state
placeholder when no wrappers are connected.

### 2. Claude button → cycle wrappers — REJECTED

Cycling lives on the rotary knob (press+rotate combo); the Claude
button (F20) is reserved for raising the active wrapper's host
terminal — see #6, which is what's actually wired up.

### 3. Mode-toggle and soft-keys go via wrapper — DONE

The daemon writes mode-toggle and soft-key bytes into the active
wrapper via `WrapperWrite` rather than the (gone) embedded terminal.
Wired through `state.rs`'s HID event dispatch.

### 4. Interactive `PermissionRequest` routing (the "80% win")

`PermissionRequest` is request/response — daemon already auto-allows
under YOLO. Extend: when not YOLO, park the HTTP response in a
`oneshot`, ship the prompt to the device with approve/reject buttons,
resolve the oneshot with the user's decision (or fall back to empty
body on timeout).

The single feature that recovers most "tap-to-control without focus
switch" value with zero terminal-control plumbing. Worth landing
standalone — independent of #1–3.

### 5. Focus detection via OSC 1004

Wrapper sends `\x1b[?1004h` on startup to enable focus reporting. A
small inline parser on the stdin → PTY path watches for `ESC [ I`
(focus in) and `ESC [ O` (focus out), forwards a
`WrapperToDaemon::FocusChanged` message. Daemon auto-updates
`active_session_id` to whichever wrapper the user is actually looking
at, replacing manual selection in the common case.

Caveats: pass-through to claude (claude ignores 1004 unless it opts
in), `\x1b[?1004l` on exit, broken on a few legacy terminals
(graceful degradation: stuck on whatever was last touched).

### 6. Raise wrapper's terminal tab on device tap

Per-terminal adapter in the wrapper. Detected at startup from env:

| Terminal | Command | Identifier |
|---|---|---|
| WezTerm | `wezterm cli activate-pane --pane-id N` | `$WEZTERM_PANE` |
| Kitty | `kitty @ focus-window --match id:N` | `$KITTY_WINDOW_ID` |
| tmux | `tmux switch-client -t %N` | `$TMUX_PANE` |
| iTerm2 | AppleScript by session id | `$TERM_SESSION_ID` + tty lookup |
| Terminal.app | AppleScript by tty | `tty()` |
| Ghostty | (app foreground only) | n/a — limited |

New `DaemonToWrapper::Focus` message; wrapper runs the right CLI on
receive. Adapter name reported in `Register` so the tray menu can
grey out "raise" for unsupported terminals.

### 7. Subscribe to more hook events

Today the daemon handles `PreToolUse`, `PostToolUse`,
`PermissionRequest`, `Stop`, `Notification`, `statusline`. Worth
wiring up:

- `SessionStart` (the http hook is installed but the handler doesn't
  do anything special — should populate session metadata eagerly,
  including `source: compact|resume|clear|startup`).
- `SessionEnd` (clean up `SessionState`; today entries just
  accumulate).
- `PreCompact` / `PostCompact` (compaction lifecycle — replaces the
  old file-watching hacks).
- `Notification(idle_prompt)` (the proper OSC-bell replacement;
  today we only check `permission_prompt`).

### 8. Static settings page on the daemon

`axum` already serves the daemon. Add `tower-http::services::ServeDir`
for static assets, plus a soft-key CRUD endpoint
(`GET/PUT /api/soft-keys/:mode/:index`) that delegates to existing HID
calls. Tray "Open Settings…" shells out to `open` (macOS) /
`xdg-open` (Linux) / `start` (Windows) to launch the user's default
browser at `http://127.0.0.1:19384/settings`.

The page hosts:
- Soft-key editor (the main reason the page exists).
- Brightness, default mode, YOLO default.
- Hooks install/uninstall buttons.
- A debug view of the wrapper registry / recent hook events.

This unblocks step 9.

### 9. Delete `crates/coredeck/` and its dep tree — DONE

Landed in three commits:

- Remove GUI crate from workspace — drops `crates/coredeck/` and
  `patches/zune-jpeg/`, points `default-members` at `coredeck-daemon`.
- Bundle the daemon as the macOS app's main executable — single binary
  in `Contents/MacOS`, single `codesign` invocation, `CFBundleExecutable
  = coredeck-daemon`.
- Update README, `docs/Building.md`, ROADMAP for the daemon-only
  layout.

`winit` stays — `tray-icon` needs an event loop on macOS. The GUI-only
deps (egui*, glutin*, glow, eframe, vte, wezterm-*, termwiz, config,
arboard, rfd) fell out automatically when the crate was removed.

### 10. Use the rest of the statusline payload

Statusline currently deserializes `context_window`, `cost`, `model`,
`session_id`, `session_name`, `effort.level`, `thinking.enabled`.
Available but ignored: `rate_limits.{five_hour,seven_day}`,
`agent.name`, `worktree.*`, `exceeds_200k_tokens`.

Display constraint: each task line on the device is ~28 chars, so
the device can't usefully accommodate more text fields. Best fit is
non-task surfaces — tray menu rows (rate-limit countdown), an alert
when `exceeds_200k_tokens` flips, or the browser settings page for
worktree / agent metadata.

### 11. `subagentStatusLine` integration — DONE

Wired up as a sibling to the regular `statusLine` hook:

- `hooks install` adds a top-level `subagentStatusLine` entry pointing
  a curl command at `POST /hooks/subagent-statusline` with stdout
  redirected to `/dev/null` so the script's empty output keeps Claude
  Code's default subagent-row rendering.
- The endpoint replaces `SessionState::subagents` wholesale on every
  refresh tick (Claude Code always sends the complete visible list).
- `WrapperTab` gained `subagent_label` / `subagent_count` and
  `push_to_device` prefers the subagent label on line 1, demoting the
  parent's `current_task` to line 2 — so the device tracks the actual
  in-flight worker rather than the parent's "Thinking…" placeholder.
- Cleared on `Stop` (turn boundary) and dropped wholesale on
  `SessionEnd`. No per-key lighting yet; that's a future extension.

Known unknowns: `startTime` shape (number vs ISO string) and exact
`status` vocabulary aren't pinned down by the docs — `coerce_start_time_unix`
accepts both number forms and `is_terminal_status` matches the obvious
terminal words. Adjust once we observe real payloads.

### 12. Optional rename: `coredeck-daemon` → `coredeck`

Once the GUI is gone, `coredeck-daemon` is the only main binary. Rename
for naturalness. Update launchd plist, bundle scripts, install docs.
Cosmetic but pleasant.

---

## Backlog / not yet committed

- **Track in-progress task across TaskCreate / TaskUpdate**: when Claude
  uses the structured Task tools (not TodoWrite), `tool_input` for
  `TaskUpdate(status: in_progress)` only carries `taskId` — the
  `activeForm` was set earlier on `TaskCreate`. The device today shows
  "Thinking…" on line 1 and bare "TaskUpdate" on line 2 instead of the
  actually-informative "Adding raise.rs module" the host UI displays.
  Fix shape: per-session `task_registry: HashMap<task_id, activeForm>`,
  populated from `PreToolUse(TaskCreate)` + `PostToolUse(TaskCreate)`
  (parse the assigned id from `tool_response`); on
  `PreToolUse(TaskUpdate, in_progress)` look up by id and surface as
  `current_task`. Clean up on Stop and SessionEnd. Confirm
  `tool_response` shape from one debug log before coding.
- **Right-anchor truncation for paths**: `extract_task_text` truncates
  `tool_input.file_path` (and similar) by lopping the *tail* — for
  long absolute paths this hides the filename, the most informative
  bit ("Edit: /Users/vden/work/agentdeck/…" vs the desired
  "Edit: …/raise.rs"). Switch to right-anchored truncation when the
  detail starts with `/` (or contains a path separator). Keep
  left-anchored for commands/queries/descriptions where the prefix is
  what matters. Bonus: while there, audit byte slicing in the same
  function for the UTF-8-boundary issue that bit `compact_text` last
  month.
- **YOLO requires device presence**: today the daemon retains
  `device_status.yolo = true` even after the HID device disconnects
  (sleep/wake, unplug, USB hub flake), so the `PermissionRequest` hook
  keeps auto-approving with no hardware affordance to confirm. Fail-safe:
  in `PermissionRequest` handler, gate auto-approve on
  `device_status.connected && device_status.yolo`. Optionally also clear
  `yolo` on `HidDisconnected` so reconnect doesn't silently re-enable.
  Real-world risk: walk away with YOLO on, device disconnects, Claude
  runs wild. Worth a short, explicit fix.
- **Hook errors when daemon is down**: hooks `POST` to
  `127.0.0.1:19384`; with the daemon stopped, Claude Code surfaces
  ECONNREFUSED noise on every hook fire. Options:
  (a) wrap each HTTP hook in a small shell shim that swallows curl
  failures (the `coredeck-register.sh` pattern) — extra fork per hook
  but bulletproof;
  (b) have the wrapper print a one-line "daemon not running, hooks
  inert" warning at startup and trust the user to know;
  (c) keep the hooks config but add a single keep-alive ping endpoint
  the wrapper checks once on register, suppressing further hook noise
  via a `CORDECK_HOOKS_SILENT=1` env var passed to claude.
  Pick after we have actual UX data on how often the daemon is offline
  in practice.
- **Wrapper resilience**: WS connect fails silently today; no
  reconnect. Add bounded backoff + reconnect so daemon restarts
  don't strand wrappers.
- **Settings install non-destructive**: current `hooks install`
  overwrites the entire `hooks` block in `~/.claude/settings.json`.
  Pre-existing user hooks are lost. Switch to deep-merge before
  public release.
- **Per-wrapper YOLO**: today YOLO is a global flag on the device. Could
  be per-session (the device shows N tabs, user can YOLO one without
  affecting the rest).
- **Resume mapping**: when a user runs `claude --resume <id>` inside a
  wrapper, the wrapper should ideally know the resumed session_id
  ahead of time (today it learns it lazily via the SessionStart hook,
  which is fine but a brief race window exists).
- **Remote daemon**: wrapper supports `COREDECK_DAEMON_ADDR`, so in
  principle you can run claude on a remote dev box and control from
  local CoreDeck. Not tested; would need TLS for anything serious.
- **Linux / Windows**: PTY + raw mode + SIGWINCH already work on Unix
  via `portable-pty` + `crossterm`. Windows needs ConPTY validation;
  Linux should be fine but unverified.
- **Installer ergonomics**: `brew install` + `coredeck setup` that
  installs the daemon, registers launchd, runs `hooks install`,
  sets up the alias. Today all manual.
- **Multiple wrappers in same cwd**: rare but possible. Today they're
  distinguished only by `wrapper_id` (correct — `cwd` was the fallback
  in the original design, now obsoleted by env-var correlation).

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

- **Display authority**: the daemon now drives the device display
  directly when at least one wrapper is connected, projecting
  `WrapperTabList` to `DisplayUpdate` in `wrapper.rs::push_to_device`.
  The GUI app's `UpdateDisplay` still works for backwards compatibility
  (e.g., when no wrapper is present) but is being phased out as part
  of the GUI deletion (step 9). After deletion, the daemon is the only
  display authority.

- **OSC alerts replaced by hooks**: `Notification(idle_prompt)` and
  `Notification(permission_prompt)` cover the alert use cases we
  cared about. The wrapper should not need an OSC parser. (If
  something else surfaces that only OSC carries, revisit.)

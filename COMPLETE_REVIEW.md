# CoreDeck — Complete Project Review

- **Date:** 2026-06-09
- **Reviewed at:** commit `4cba959` (branch `main`)
- **Scope:** the `app` workspace — `crates/coredeck`, `crates/coredeck-claude`,
  `crates/coredeck-protocol`, `docs/`, shell scripts, macOS packaging,
  Homebrew cask, GitHub workflows, tests. Firmware was consulted only to
  verify limits quoted in `docs/Protocol-Limits.md`.
- **Method:** six parallel review passes (WS/protocol docs vs code, REST docs
  vs code, README/ROADMAP/build docs vs reality, daemon deep review,
  wrapper + HID deep review, build/CI/packaging/tests), with every
  high-severity finding independently re-verified against the source before
  filing.

Findings are numbered for reference (S = security, C = daemon correctness,
W = wrapper, H = HID, P = protocol, D = docs, R = README/ROADMAP/build docs,
B = build/CI/packaging, X = design/dead code, T = tests). Severity is the
practical impact today, not theoretical worst case.

> **Fix status (2026-06-09, same session): ALL findings below are
> resolved.** The work landed in batches, each gated on
> `cargo fmt --check` + `cargo clippy --workspace --all-targets` +
> `cargo test --workspace` (69 tests, all green), plus `shellcheck` on
> every script and YAML validation of both workflows. New regression
> tests were added for the loopback guard, the hook helpers
> (`references_coredeck`, `shell_single_quote`, `coerce_start_time`),
> the wrapper parsers (`FocusStripper`, `OscSniffer`, `keep_title_hint`,
> `local_daemon_port`), and the shared `session_label`.
>
> A few items were resolved by **documenting a deliberate trade-off**
> rather than changing behavior, where a code change wouldn't have
> improved things: **C8** (the legacy `permission_prompt` WS path has no
> tool identity to disambiguate parallel tools — commented), **X5** (the
> `statusLine` hook necessarily blanks the terminal status line to feed
> the device — commented), **X6/X7** (acknowledged unbounded-growth /
> leak-on-restart trade-offs — left as-is). **B13** (cask
> `sha256 :no_check`) stays a documented release-time tap fixup. The
> ProtoError mapping (**H7/X3**) surfaces firmware `Error` responses;
> generic non-zero status on *normal* responses is intentionally not
> treated as an error since that byte is command-specific.
>
> The detailed per-finding notes below are retained as the historical
> record of what was found; treat the whole document as closed unless a
> finding's own text says otherwise.

---

## Executive summary

| Area | High | Medium | Low |
|---|---|---|---|
| Security | 3 | — | 1 |
| Daemon correctness | 1 | 4 | 6 |
| Wrapper (`coredeck-claude`) | 2 | 2 | 6 |
| HID layer | 2 | 4 | 3 |
| Protocol (code-internal) | — | 2 | 3 |
| API/protocol docs vs code | 6 | 10 | 14 |
| README/ROADMAP/build docs | — | 7 | 7 |
| Build / CI / packaging | 3 | 9 | 12 |
| Design / dead code | — | 1 | 6 |
| Tests | 1 | 1 | — |

**The ten findings that matter most:**

1. **The signed macOS release pipeline cannot ship a good build** — two
   independent breaks: the "Ad-hoc sign" step always runs and clobbers the
   Developer ID signature (B1), and `create-dmg.sh` rejects the `--apple-id`
   flag `release.yml` passes it (B2).
2. **Linux HID writes omit the hidraw report-ID byte** — any standalone-mode
   message ≥3 chunks is corrupted on the wire (H1).
3. **Linux unplug detection never fires** — udev `remove` events are matched
   via sysfs attributes that are already gone, so `DeviceRemoved` is never
   forwarded (H2).
4. **Unauthenticated localhost API + `CORS Any` allows keystroke injection**
   into the active Claude PTY (`WrapperWrite` over `/ws`) and **persistent
   payloads in device EEPROM** (soft-key endpoints) from any local process or
   a malicious web page (S1–S3).
5. **UTF-8 byte-slice panic in hook logging** on attacker-influenceable
   payloads (C1).
6. **`tests/integration/` is dead** — never compiled, imports a deleted crate
   (T1); meanwhile the most behavior-dense modules have zero tests (T2).
7. **The WS protocol docs are missing three live message types**
   (`0x0B WrapperWrite`, `0x0C SetActiveWrapper`, `0x8A WrapperTabList`), and
   `GET /api/wrappers` returns a completely different shape than documented
   (D1, D8).
8. **`--ssh` reuses a persistent wrapper_id and the daemon unregisters by id
   unconditionally** — a second invocation against the same host kills the
   first, live wrapper's registration (W2).
9. **The wrapper leaves the terminal in raw mode** (with OSC 1004 focus
   reporting still on) on SIGTERM/SIGHUP/panic (W1).
10. **WS tag collision:** `WsEventTag::ClaudeHookEvent` and
    `WsResponseTag::SoftKeyResponse` are both `0x85`, disambiguated only by
    an unenforced "commands use seq > 0" convention (P1).

---

## 1. Security

The daemon binds `127.0.0.1:19384` with no authentication. That is fine for
purely observational APIs, but this API can type into terminals and program
hardware, and the HTTP layer actively invites cross-origin callers.

- **S1 (High)** — `crates/coredeck/src/main.rs:472-477`: the axum router
  installs `CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any)`,
  and neither the HTTP handlers nor the two WS upgrade handlers validate
  `Origin`/`Host`. Browsers permit cross-origin WebSocket connects and
  (with this CORS policy) XHR to loopback, so a malicious web page — not just
  a local process — can drive every endpoint (DNS-rebinding / CSRF surface).
- **S2 (High)** — `crates/coredeck/src/ws.rs:271-281`: a `/ws` client holding
  the (first-come) lock can send `WrapperWrite` (`0x0B`), which routes
  arbitrary bytes — including `\r` — into the active wrapper's PTY via
  `wrapper::write_to_target`. Combined with S1 this is effectively local code
  execution in the user's shell session. `SetActiveWrapper` (`0x0C`) lets the
  attacker pick the target.
- **S3 (High)** — `crates/coredeck/src/rpc.rs:369-412` and `:578-639`:
  `PUT /api/soft-keys/{index}` and `POST /api/soft-keys/presets/apply` accept
  a `String`-type key with arbitrary bytes and persist it to device EEPROM
  (`save: true`). A cross-origin page can program a soft key to type
  `curl … | sh\r`; the payload survives reboots and fires when the user
  presses the physical key.
- **S4 (Low)** — `crates/coredeck/src/raise.rs:128-145` and `:176`:
  `raise_iterm2` interpolates the `ITERM_SESSION_ID`-derived uuid and
  `raise_jetbrains` interpolates the bundle id into AppleScript without
  `applescript_quote`. Both values are terminal-controlled (low risk), but a
  `"` in either breaks the script — and it is inconsistent with `spawn.rs`,
  which quotes correctly on the equivalent paths.

**Suggested direction:** validate `Origin` (reject browser-originated
requests outright), drop `CorsLayer(Any)` (the settings page is same-origin
and needs no CORS), and consider a per-install bearer token for the mutating
endpoints and `/ws`.

---

## 2. Daemon correctness (`crates/coredeck`)

- **C1 (High)** — `hooks.rs:223-224`: `info!("HOOK {}: {}...", event_type, &json[..200])`
  slices serialized hook JSON at **byte** 200. Hook bodies routinely carry
  multi-byte UTF-8 (prompts, titles, paths); a code point straddling byte 200
  panics the request task and the hook is lost. Use a char-boundary-safe
  truncation (the codebase already has `truncate_chars`-style helpers).
- **C2 (Medium)** — `hooks.rs:1233-1237`: after the 300 s parked-
  PermissionRequest timeout race, the handler calls `alerts::clear_alert`
  **unconditionally**. On the normal device-resolve path the alert was
  already consumed and the next queued prompt (B) promoted — so the wake-up
  clears B and drops B's oneshot, sending B's request back to the terminal
  even though it was validly on the device. This defeats the pending-queue
  for the common "two parallel permission prompts" case. Condition the clear
  on the live alert still belonging to this request.
- **C3 (Medium)** — `alerts.rs:269-326` and `:217-248`: TOCTOU between the
  "busy" check and alert installation. `install_pending_alert` checks
  `alert_state.is_some()`, drops the lock for the HID send, then writes
  `AlertState::Pending` — a concurrent installer in that window is silently
  overwritten (dropping its oneshot). `show_idle_alert` has the same shape,
  so an idle prompt can clobber a pending permission prompt.
- **C4 (Medium)** — `alerts.rs:658-728`: `consume_input_for_decision`
  snapshots the alert under one lock acquisition and `mem::take`s it under a
  second; a promotion racing in between (Stop hook → `cancel_pending_for_session`
  → `try_install_next_pending`) can swap prompts so the keypress classified
  for prompt A resolves prompt B.
- **C5 (Medium)** — `main.rs:662, 675, 679`: F20/F24 handling awaits
  `raise::*`/`spawn::*` inline on the single HID-event-loop task, and those
  run external commands (`osascript`, `wmctrl`, `gdbus`, terminal CLIs,
  `ssh`) via `Command::output().await` with **no timeout**
  (`raise.rs:432-447`, `spawn.rs:360-375`). One hung helper freezes all
  further HID input processing. Wrap in `tokio::time::timeout` and/or spawn
  off the event loop.
- **C6 (Medium)** — `hooks.rs:1968-1969, 2012-2013, 2077, 2085, 2144-2146`:
  hook entries are *written* using the real `base_url` (respecting
  `--listen`), but `is_managed_hook_block`, `warn_if_clobbering`,
  `uninstall_hooks_result`, and `are_hooks_installed` match the literal
  `127.0.0.1:19384`/`localhost:19384`. With a custom `--listen`, uninstall
  leaves the `statusLine`/`subagentStatusLine` keys behind and the installed
  check misreports.
- **C7 (Low)** — `main.rs:282` → `hooks.rs:2156-2161`: startup tray seeding
  calls `are_hooks_installed()` → `claude_settings_path()` →
  `env::var("HOME").expect(...)` — daemon panics at startup if `HOME` is
  unset.
- **C8 (Low)** — `hooks.rs:1182-1190`: `pending_permissions` is keyed by
  `session_id` only; parallel PermissionRequests in one session overwrite
  each other's `tool_name`/`tool_input`, so the legacy
  `Notification(permission_prompt)` → WS path (`hooks.rs:1374-1382`) can
  render the wrong tool's details.
- **C9 (Low)** — `hooks.rs:381-390`: `mode_changed_for_active` compares
  `prev != event.permission_mode` even when the event carries no
  `permission_mode` (`None`), so it reports "changed" without any state
  change. Harmless today (the sync is idempotent) but a wrong comparison.
- **C10 (Low)** — `hooks.rs:343-353`: `coerce_start_time_unix` claims to
  accept ISO-8601 strings; the `Value::String` arm returns `None` — the
  branch is dead and ISO timestamps are silently dropped.
- **C11 (Low)** — `alerts.rs:280-295` + `hooks.rs:1233`: a queued
  `QueuedPending` whose HTTP handler already timed out stays in
  `pending_queue` with a dead sender; when promoted it shows a device prompt
  nobody is waiting on. Usually evicted by the next hook for that session,
  so the window is bounded.

---

## 3. Wrapper (`crates/coredeck-claude`)

- **W1 (High)** — `main.rs:666` enables raw mode; cleanup runs only on the
  happy path (`main.rs:821-830`). The only signal handled anywhere is
  SIGWINCH (`main.rs:781-804`). SIGTERM/SIGHUP terminate the process without
  `disable_raw_mode()` or the OSC 1004 disable write — the user's shell is
  left raw with focus reports spraying `^[[I`/`^[[O`. Panics likewise bypass
  cleanup (e.g. the `expect("master lock poisoned")` at `main.rs:653/658`);
  `main()` only catches `Err`, not unwinds. Install a panic hook plus
  SIGTERM/SIGHUP handlers (or a scope guard).
- **W2 (High)** — `main.rs:394-411` + daemon `wrapper.rs:140-156, 238-241`:
  every `--ssh <host>` invocation reuses the same persisted wrapper_id. A
  second concurrent invocation (1) overwrites the first wrapper's registry
  entry on Register, (2) dies on the tunnel port collision
  (`ExitOnForwardFailure`, `main.rs:527-529`), and (3) its WS close triggers
  the daemon's **unconditional** `wrappers.remove(&wrapper_id)` — deleting
  the first, still-live wrapper, which never re-registers until its own
  connection flaps. The same remove-without-identity-check races ordinary
  reconnects (late close of a half-open socket deletes the fresh entry). Fix:
  per-connection token in `Wrapper`, remove only if it matches.
- **W3 (Medium)** — `main.rs:680, 856-876, 946-969`: the unbounded
  `WrapperEvent` channel is only drained while connected. Claude updates the
  terminal title several times per second; with the daemon down for hours the
  backlog grows to megabytes, and on reconnect the entire history — including
  stale `FocusIn`/`FocusOut` — replays into the daemon, churning
  active-wrapper promotion and alert clearing. Coalesce (keep latest per
  kind) or drain while disconnected.
- **W4 (Medium)** — `main.rs:1-5`: `#![windows_subsystem = "windows"]` on a
  console PTY wrapper. On Windows release builds the process detaches from
  the console, killing the entire stdin↔PTY proxy. (Windows is roadmap-only,
  but this attribute belongs on the tray daemon, not here.) Relatedly the
  resize path is `#[cfg(unix)]` only — no ConPTY resize at all.
- **W5 (Low)** — `main.rs:60-83`: `extract_focus_events` only matches
  `ESC [ I/O` contiguous within one `read()`; a report straddling the 4096-
  byte boundary is forwarded to claude as literal bytes and the event lost.
  A 1–2 byte carry-over state machine would fix splits without the bare-ESC
  pitfall the comment worries about. (Otherwise the parser is correct — exact
  3-byte match, no CSI corruption.)
- **W6 (Low)** — `main.rs:550-556`: when `run()` returns `Err`, `main()`
  restores raw mode but never writes `FOCUS_REPORTING_DISABLE` (that write
  is happy-path only, `main.rs:821-827`) — the host terminal keeps emitting
  focus reports at the shell after exit.
- **W7 (Low)** — `main.rs:973-986`: backoff is reset to a fixed 1 s after any
  *established* connection drops; only connect-failures grow 1 s→30 s. A
  daemon that accepts then immediately closes is hammered at 1 Hz forever —
  a minor deviation from the documented "bounded exponential backoff".
- **W8 (Low)** — `main.rs:348-355`: a trailing `--ssh` with no host is
  silently dropped → wrapper runs claude *locally* instead of erroring.
  Also `main.rs:394-426`: the wrapper-id cache file is written non-atomically
  with no locking; two concurrent first-runs can race.
- **W9 (Low)** — `main.rs:430-436`: the SSH tunnel port falls back to a
  hardcoded `19384` instead of parsing `DEFAULT_DAEMON_ADDR`
  (`coredeck-protocol/src/lib.rs:288`); the two can silently diverge.
- **W10 (Low)** — `main.rs:51, 686-687`: comments say the title sniffer is
  "OSC 9", but the implementation filters `0 | 1 | 2` (`main.rs:710`) and OSC
  9 is explicitly *not* a title. Same stale claim on `TitleHint`
  (`coredeck-protocol/src/lib.rs:573-576`) and `WrapperTab::terminal_title`
  (`lib.rs:619`). The docs (`Types.md`, ROADMAP) say OSC 0/1/2 and are right;
  the code comments are wrong. Nit: a title body truncated mid-code-point at
  the 512-byte cap fails `from_utf8` (`main.rs:713`) and is dropped instead
  of lossily truncated.

---

## 4. HID layer (`crates/coredeck/src/hid`)

- **H1 (High, verified)** — `device.rs` `send_single_packet`
  (~`:1116-1133`): macOS/Windows prepend the `0x00` report-ID byte; the
  `#[cfg(target_os = "linux")]` branch sends the bare 32 bytes. hidraw's
  `write(2)` contract is platform-independent: first byte is the report
  number, `0` for unnumbered reports, and the kernel strips it. Net effect on
  Linux: packets whose first (flags) byte is nonzero (START `0x80`, END
  `0x40`, single-chunk `0xC0`) happen to transit intact, but standalone-mode
  *middle* chunks have flags `0x00` — the kernel eats the flags byte and the
  report arrives shifted. **Every standalone-mode message ≥3 chunks (any
  display-update JSON over ~60 bytes) is corrupted.** VIAL mode masks the bug
  because wire byte 0 is always `0x80`. Fix: prepend `0x00` on Linux too.
- **H2 (High)** — `hotplug_linux.rs:180-201`: `matches_device` filters
  `remove` events via `attribute_value("idVendor"/"idProduct")` — sysfs
  reads that are already torn down when a removal uevent reaches userspace —
  so it returns `false` and `DeviceRemoved` is **never** forwarded. Unplug is
  only noticed if the device was open (3× ping failure); otherwise
  `device_available` stays `true` and the tray reports a phantom device until
  replug. Match on event *properties* instead (e.g. `PRODUCT=feed/803/…` via
  `property_value`), which survive removal. This is the main behavioral
  parity break with the macOS watcher.
- **H3 (Medium)** — `device.rs:1277-1279`: `while payload.last() == Some(&0) { pop }`
  trims zero-padding but also eats legitimate trailing `0x00` data: a theme
  dump whose last slot is black fails "theme dump truncated"
  (`device.rs:1172-1196`); a `reset_soft_keys` response whose last keycode
  low byte is `0x00` drops that entry (`device.rs:760-777`). Needs a length
  field or per-command fixed lengths.
- **H4 (Medium)** — chunked `TypeString` reassembly state is split across
  three independent buffers (reader thread `device.rs:337`, `drain_response`
  `:940`, `read_response_with_timeout` `:1224`); a multi-packet TypeString
  interleaved with a host command gets its chunks routed to unrelated
  buffers and is truncated/corrupted. Soft-key strings are exactly the
  multi-chunk case.
- **H5 (Medium)** — `device.rs:172-197`: after a macOS arrival event the code
  sleeps 100 ms, checks presence — then emits `DaemonEvent::DeviceAvailable`
  **even when the check failed**, leaving consumers told "available" while
  `is_device_available()` is false and nothing retries. Slow enumeration
  (hubs, composite devices) misses the device until physical replug.
- **H6 (Medium)** — `hotplug_macos.rs:189, 238-249`: devices found while
  arming the IOKit notification (the `drain_iterator` pass) are only
  debug-logged. A device plugged between `HidManager::new`'s enumeration and
  the notification arm falls into the gap forever. One-line fix: emit
  `DeviceArrived` from the drain — the consumer already dedups.
- **H7 (Low)** — firmware error statuses are ignored on several commands:
  `set_soft_key` discards the response (`device.rs:650-657`) and logs
  success regardless; `get_soft_key` never checks `response.status`
  (`:683-689`); display/brightness/alert/mode go through `drain_response`
  which discards everything. `ProtoError` exists for exactly this and is
  never used (see X3).
- **H8 (Low)** — `device.rs:516-521`: `connected.load()` → `try_connect()`
  with no interlock; the wrapper handler and WS handler can both pass the
  check and double-open, the second open dropping the first handle and
  emitting a duplicate `HidConnected`. Transient, self-healing.
- **H9 (Low)** — `HidConfig::reconnect_interval_ms` (`main.rs:45,56`) is
  dead: the monitor uses hardcoded `RECONNECT_INITIAL_MS`/`RECONNECT_MAX_MS`
  (`device.rs:33-34`) with an undocumented ×1.5 growth. Wire it through or
  remove it.

---

## 5. Protocol — code-internal (`crates/coredeck-protocol`)

- **P1 (Medium, verified)** — `lib.rs:192` vs `lib.rs:220`:
  `WsEventTag::ClaudeHookEvent = 0x85` collides with
  `WsResponseTag::SoftKeyResponse = 0x85`. Both are daemon→client frames;
  disambiguation relies solely on hook events using `seq == 0` and clients
  never sending `seq == 0` commands — which nothing enforces
  (`ws.rs:131-149` happily echoes seq 0). Event tags otherwise skip the
  response range (`AppControl` jumps to `0x89`), so this looks like an
  oversight. Renumbering requires lockstep with clients.
- **P2 (Medium)** — `ws.rs:255-269`: the `ClearAlert` JSON-payload
  alternative is dead code — the JSON branch is only reached when the payload
  is *empty* (where parsing always fails); a non-empty JSON payload like
  `{"tab":0}` takes the raw-byte branch and clears tab `0x7B` (123). Both
  WebSocket-Protocol.md:163 and Types.md:104 document the JSON form as
  working (see D-section).
- **P3 (Low)** — `lib.rs:565-568`: `WrapperToDaemon::Goodbye` is defined and
  handled by the daemon (`wrapper.rs:204-207`) but never sent — child exit
  just aborts the WS task (`coredeck-claude/src/main.rs:815-816`). Send it
  (it carries `exit_code`) or delete it.
- **P4 (Low)** — `lib.rs:539-562`: the `Register` handshake carries no
  protocol/binary version. A newer wrapper sending an unknown first variant
  is silently dropped by the daemon (`wrapper.rs:55-61`) and retries forever
  at 1 s. The `Registered` ack is the natural place for a daemon version.
- **P5 (Low)** — `FocusChanged.wrapper_id`/`TitleHint.wrapper_id` are ignored
  by the daemon in favor of the connection-scoped id (`wrapper.rs:204-224`)
  — redundant wire bytes; fine, but worth documenting as such.

---

## 6. API / protocol docs vs implementation

### docs/WebSocket-Protocol.md

- **D1 (High)** — Three live message types are entirely undocumented:
  command `0x0B WrapperWrite` (`lib.rs:156`, `ws.rs:271-281`), command
  `0x0C SetActiveWrapper` (`lib.rs:159`, `ws.rs:282-290`), and event
  `0x8A WrapperTabList` (`lib.rs:197`, emitted on every wrapper/hook/tab
  change — high-volume traffic every `/ws` client receives,
  `wrapper.rs:326-333`). The doc's command list and tag summary stop at
  `0x0A`/`0x89`.
- **D2 (Medium)** — Line 22 "on disconnect the HID device interface is
  closed" is stale: `ws.rs:120-127` only clears the lock;
  `rpc.rs:3-9` documents the retired transient-open model — the daemon keeps
  the handle open for its lifetime.
- **D3 (Medium)** — Line 163's "alternatively JSON-encoded
  `ClearAlertRequest`" doesn't work (see P2).
- **D4 (Medium)** — Line 242's `ClaudeHookEvent.event` list omits the
  daemon-synthesized `"permission_prompt"` envelope (`hooks.rs:1388-1402`).
- **D5 (Low)** — Lines 43/169 "commands must use seq > 0" is unenforced
  (`ws.rs:131-149`) — and load-bearing given P1.

### docs/Types.md

- **D6 (High)** — Line 285: `WrapperTabList` is *not* what
  `GET /api/wrappers` returns — the endpoint returns a bare array of ad-hoc
  debug rows (`{wrapper_id, pid, cwd, started_at_unix, session_id,
  host_terminal_kind, active}`, `rpc.rs:723-757`), with none of the
  documented `WrapperTab` fields and no envelope.
- **D7 (Medium)** — Line 285 also says WrapperTabList is broadcast "as part
  of ClaudeHookEvent activity" — it's a dedicated `0x8A` event, never inside
  a `0x85` envelope, and WebSocket-Protocol.md documents no broadcast at all.
  Three-way contradiction (Types.md vs WebSocket-Protocol.md vs code).
- **D8 (Medium)** — `WrapperTab.is_remote` (`lib.rs:671-675`) is missing from
  the field table and example.
- **D9 (Low)** — The `WrapperTab` example shows `null`/`0` for optional
  fields that are actually omitted from serialization
  (`skip_serializing_if`, `lib.rs:665-670`).

### docs/Protocol-Limits.md

- **D10 (High)** — Brightness defaults are wrong: doc says default 255 /
  dimmed 178; firmware says default **178** (`DISPLAY_BL_DEFAULT_LEVEL`,
  reduced for the high-VLT cover) / dim **125** (`DISPLAY_BL_DIM_LEVEL`)
  (`firmware/keyboards/core_deck/rev1/config.h:117-118`). The doc appears to
  have promoted the old default into the dim slot.
- **D11 (Low)** — Line 71: 30-byte chunks are standalone-mode only; VIAL mode
  chunks are 29 bytes (`hid/protocol.rs:24-43`).
- **D12 (Low)** — Line 93 "all *mutating* endpoints return 409" is
  incomplete: `GET /api/version`, `GET /api/soft-keys`, `GET /api/theme`
  also 409 under the WS lock (`rpc.rs:252-260, 330-336, 459-466`).

### docs/API.md

- **D13 (Medium)** — Line 19 "mutating endpoints transiently open the HID
  device per request" describes the retired model; the handle is persistent
  (`rpc.rs:3-9, 28-41`).
- **D14 (Medium)** — Line 17's lock-state summary is wrong in both
  directions: three GETs do 409 under lock (D12) while
  `GET /api/wrappers`, `GET /api/hooks/status`, `GET /api/soft-keys/presets`
  always work.
- **D15 (Low)** — "Three ways to communicate" omits `/wrapper-ws` +
  `/wrapper/register` and the embedded settings page (`main.rs:454-471`).

### docs/REST-API.md

- **D16 (High)** — `GET /api/soft-keys`: doc shows a `{"keys": [...]}`
  envelope; the handler returns a bare array (`rpc.rs:344-365`;
  settings.html:830-832 consumes the bare array).
- **D17 (High)** — `POST /api/soft-keys/presets/save`: documented body
  `{"name": …}` would be rejected with 422 — `SavePresetRequest.keys:
  [PresetKey; 3]` is required (`rpc.rs:643-649`); the description wrongly
  implies the server captures current keys.
- **D18 (High)** — `GET /api/hooks/status`: doc includes `settings_path`;
  the handler returns only `{"installed": bool}` (`rpc.rs:275-277`).
- **D19 (Medium)** — `POST /api/soft-keys/reset` returns the post-reset
  config array, not an empty body (`rpc.rs:437-438`).
- **D20 (Medium)** — `GET /api/soft-keys/presets` actually returns
  `{"builtin": [...], "user": [...]}` including hardcoded built-ins; doc
  describes only "user-defined groupings" with no shape (`rpc.rs:561-567`).
- **D21 (Medium)** — `presets/apply` example body includes `save`, which the
  request type doesn't have — silently ignored (`rpc.rs:571-574`).
- **D22 (Medium)** — `/api/hooks/install`+`uninstall` return empty bodies,
  not "the new install status"; failures return undocumented 500 + ApiError
  (`rpc.rs:287-310`). Install also writes `coredeck-register.sh` and
  overwrites the `statusLine`/`subagentStatusLine` keys — only the curl shim
  is documented (`hooks.rs:1618-1650`).
- **D23 (Medium)** — The interactive PermissionRequest flow is undocumented:
  the POST can block up to 5 minutes, and can answer with a deny payload or
  `{}` passthrough — the doc shows only the YOLO auto-allow shape, and omits
  that auto-approve also requires device-connected + per-wrapper enrollment
  (`hooks.rs:1133-1291`).
- **D24 (Medium)** — `POST /wrapper/register` exists in the router
  (`main.rs:468-471`) and is the linchpin of session correlation, but is
  documented nowhere in REST-API.md (API.md mentions only `/wrapper-ws`).
- **D25 (Low)** — Undocumented error responses: soft-key index 400,
  preset-apply 404, preset-save 400s (empty name, builtin collision),
  preset-delete 400/404 (`rpc.rs:374-381, 590-598, 652-671, 691-710`);
  unknown `/hooks/{event}` types return 200 (`hooks.rs:500-503`);
  `GET /api/version` never 500s — device errors yield
  `{"version":"unknown"}` (`device.rs:582-605`); 503 bodies aren't always
  `{"error": "Device not available"}` (`rpc.rs:33-41`).
- **D26 (Low)** — `PUT /api/theme/{slot}` doc claims the settings page sends
  `save=false` during drags — the current page is local-only while dragging
  and PUTs dirty slots once on Save (settings.html:1383-1437); the comment
  in `rpc.rs:452-455` is equally stale.

---

## 7. README / ROADMAP / build docs vs reality

- **R1 (Medium)** — README.md:83: the Linux from-source dependency list
  (`build-essential pkg-config libudev-dev libhidapi-dev`) is missing the
  tray stack — CI itself installs `libgtk-3-dev libxdo-dev
  libayatana-appindicator3-dev` (`ci.yml:71-73`; Building.md:29-42 has it
  right). A clean box fails the README path.
- **R2 (Medium)** — ROADMAP.md:30-31 claims tray tab-row click "set[s]
  active **and raise[s] the host terminal**"; the `FocusWrapper` path
  (`main.rs:349-362` → `set_active_wrapper`) only sets active and syncs the
  mode LED — no raise call.
- **R3 (Medium)** — ROADMAP.md:155-156 "the cask `--zap` clears hook
  config": the zap stanza (`macos/Casks/coredeck.rb:33-38`) trashes the
  launchd plist, log, and the two shim scripts but **leaves the hook entries
  in `~/.claude/settings.json`** pointing at deleted scripts; it also misses
  the daemon data dir (`~/Library/Application Support/com.coredeck.CoreDeck/`
  — wrapper ids, presets; `wrapper_state.rs:86-88`, `presets.rs:229-230`).
- **R4 (Medium)** — The device **theme editor** (routes `/api/theme*`,
  `main.rs:423-428`; a large chunk of settings.html; commits `ff98ed1`,
  `0eaa406`) and the **soft-key presets API** (`main.rs:434-449`) are
  shipped but absent from README and ROADMAP's Done section — violates the
  project's ship-docs-with-code convention.
- **R5 (Medium)** — Building.md:127-134 "Both binaries use tracing with
  `RUST_LOG`": the wrapper reads `COREDECK_LOG`, not `RUST_LOG`
  (`coredeck-claude/src/main.rs:561-570`). `RUST_LOG=debug cargo run -p
  coredeck-claude` does nothing.
- **R6 (Medium)** — macos/BUILD.md:77-80 still describes the deleted
  universal-binary (Intel + lipo) release flow; release.yml builds arm64
  only. README/ROADMAP state the drop correctly; BUILD.md wasn't updated.
  BUILD.md's version-bump checklist (222-227) also omits the cask version
  and the other two crate manifests.
- **R7 (Medium)** — `config/default.toml` is loaded by **nothing** (zero
  references in code/scripts/workflows) and contradicts the hardcoded
  defaults (`ping_interval_ms` 2000 vs code 5000; `reconnect_interval_ms`
  1000 vs code 2000 — `main.rs:48-58`); its `[terminal]`/`[claude]` sections
  describe the deleted GUI-era design. Delete or wire up.
- **R8 (Low)** — README.md:104-106 "HTTP hook entries": the installer writes
  `type: "command"` entries invoking the curl shim (`hooks.rs:1864-1924`) —
  not Claude Code HTTP-type hooks.
- **R9 (Low)** — ROADMAP's architecture/hook-coverage lists omit
  `TaskCreated`/`TaskCompleted`, which the daemon handles
  (`hooks.rs:484-491`) and registers (`hooks.rs:1846-1850`).
- **R10 (Low)** — Building.md:49: on Linux the tray runs under
  `gtk::init()`/`gtk::main()` (`main.rs:225-228, 827-869`), not
  "tray-icon/winit".
- **R11 (Low)** — linux-setup.md:149-159 "KWin Scripting via gdbus (always
  works)": the KWin path is only attempted for GnomeTerminal/Konsole/
  Alacritty host kinds (`raise.rs:88-90, 198-219`); `Unknown` terminals on
  KDE Wayland without `WINDOWID` get nothing.
- **R12 (Low)** — `main.rs:1019` comment references a nonexistent
  `coredeck quit` subcommand.
- **R13 (Low)** — Undocumented (deliberately?) env surface: `COREDECK_LOG`,
  `COREDECK_CLAUDE_BIN`, `COREDECK_DAEMON_ADDR`, `COREDECK_SSH_HOST` — only
  `COREDECK_LOG` collides with a wrong doc claim (R5); the rest are simply
  invisible.

Verified-true doc claims worth noting: tarball naming + `install.sh`
behavior match release.yml exactly; port 19384 is consistent everywhere;
every README CLI command exists; entitlements really did lose the `cs.*`
relaxations; update-checker repos/timing match `updates.rs`; F20/F24
keycodes, launchd label, systemd unit path, SessionBound echo, `↦` remote
marker, JetBrains watcher details all check out.

---

## 8. Build / CI / release / packaging

### Release pipeline

- **B1 (High, verified)** — `release.yml:165`: the "Ad-hoc sign" step's
  condition `if: ${{ env.MACOS_CERTIFICATE == '' }}` references an env var
  that is **only defined in other steps' `env:` blocks** — at this step it
  is always empty, so the step always runs and `codesign --force --deep
  --sign -` clobbers the just-created Developer ID signature before DMG
  creation/notarization. Signed releases ship ad-hoc-signed (or fail
  notarization).
- **B2 (High, verified)** — `release.yml:177-181` passes `--apple-id
  "$APPLE_ID"` to `create-dmg.sh`, whose parser (`create-dmg.sh:39-71`)
  accepts only `--identity/--team-id/--keychain-profile/--skip-notarize/
  --output` and **exits 1 on unknown options**. Any tagged release with
  `SIGNING_IDENTITY` set fails at "Create DMG". (The script reads
  `$APPLE_ID` from the environment, which the step also sets — dropping the
  flag is the minimal fix.)
- **B3 (Medium)** — `create-dmg.sh:13` derives the DMG version from
  `crates/coredeck/Cargo.toml` while the release/artifact version comes from
  the tag (`release.yml:186-195`), with no consistency assertion. A tag
  without a Cargo bump produces a mismatched DMG name, a 404ing cask URL,
  and an update-checker (`updates.rs:69`, `CARGO_PKG_VERSION`) that loops on
  "update available".
- **B4 (Medium)** — `install.sh` — the first code every Linux user runs —
  exists only as a heredoc inside `release.yml:256-281` (udev rules likewise
  at 249-254): unreviewable by shellcheck, untestable. Move into the repo
  and `cp` in.
- **B5 (Low)** — `release.yml:73-75` `strip` step is redundant
  (`Cargo.toml:16` already sets `strip = true`); no test gate runs on tag
  push.

### CI

- **B6 (High)** — `ci.yml:27-30`: the macOS job's `cargo test`/`cargo
  clippy` run **without `--workspace`**, and the root manifest sets
  `default-members = ["crates/coredeck"]` — so `coredeck-claude` is never
  clippy-checked or tested anywhere (Linux runs `cargo test --workspace` but
  has no clippy step), and `coredeck-protocol`'s tests never run on macOS.
- **B7 (Medium)** — No dependency caching in either workflow (4 cold builds
  of ~439 crates per run; Linux additionally compiles release + debug).
- **B8 (Medium)** — Declared MSRV (`rust-version = "1.75"` in all three
  manifests, `clippy.toml msrv`) is never checked — CI is stable-only.
- **B9 (Low)** — No `--locked` on any cargo invocation; no
  `cargo audit`/`cargo deny`; no shellcheck job despite two scripts being
  embedded into the binary via `include_str!` (`hooks.rs:1603,1608`);
  `ubuntu-22.04-arm` runner labels are public-repo-only.

### Packaging

- **B10 (Medium)** — `macos/Casks/coredeck.rb` has no
  `depends_on arch: :arm64`; the DMG is arm64-only, so Intel users get a
  clean install of a non-runnable app.
- **B11 (Medium)** — Version is hand-maintained in 6 places (3 crate
  manifests — no `[workspace.package]` inheritance — Info.plist ×2 keys,
  cask) with no single source of truth; `bundle.sh` copies Info.plist
  verbatim without stamping the Cargo version.
- **B12 (Low)** — `entitlements-appstore.plist` is both over-privileged
  (retains `cs.allow-unsigned-executable-memory`) and under-privileged
  (lacks `network.server`, without which the daemon can't bind under App
  Sandbox). Declared out of scope in BUILD.md, but as kept it's wrong in
  both directions.
- **B13 (Low)** — Cask `sha256 :no_check` with a pinned version fails
  `brew audit`; documented as a template to be fixed up in the tap — one
  more manual release step.

### Shell scripts

- **B14 (Medium)** — `hooks.rs:1864,1893` build hook commands as unquoted
  `"<path> <event>"` strings — a `$HOME` containing spaces (legal on macOS)
  breaks every installed hook.
- **B15 (Medium)** — `coredeck-hook.sh` has no `curl -f`: a daemon 4xx/5xx
  body passes to stdout with exit 0, and for PermissionRequest Claude Code
  will try to parse an error page as the permission envelope. (The
  swallow-ECONNREFUSED contract and the optional max-time argument are
  correctly implemented otherwise.)
- **B16 (Low)** — `coredeck-register.sh`: the no-jq fallback
  (`grep -o '"session_id":"[^"]*"'`) breaks on whitespace after the colon;
  `SESSION_ID`/`WRAPPER_ID` are interpolated into the JSON body unescaped.
  `MAX_TIME` in the hook shim is not validated numeric.
- **B17 (Medium)** — `scripts/generate-tray-icons.sh:8-9` points at
  repo-root `assets/icons/`, which doesn't exist (the icons moved to
  `crates/coredeck/assets/icons/`) — the script unconditionally exits
  "Source file not found". Stale since the crate move; the root `assets/`
  dir is an empty husk.
- **B18 (Low)** — `macos/scripts/*` use `set -e` but not `-u`/`pipefail`;
  `create-dmg.sh` fails mid-run if `APPLE_ID` is set but `TEAM_ID` empty;
  `setup-notarization.sh` uses `read -p` without `-r`. (Path quoting,
  including the embedded space in `Core Deck.app`, is consistently correct.)

### Repo hygiene

- **B19 (Medium)** — root `build.rs` is attached to the **virtual** workspace
  manifest and never executes (no package, no `custom-build` target). Its
  placeholder-PNG generator is dead; were it ever moved into a crate it
  writes into the source tree instead of `OUT_DIR`. Delete or relocate.
- **B20 (Low)** — Untracked design sources at repo root (`heroshot.xcf`,
  `sticker*.xcf/png/pdf`) are one `git add .` away from being committed —
  gitignore or relocate. `macos/AppIcon.iconset/` (generated) isn't ignored;
  `AppIcon.icns` is committed yet regenerated by the release workflow.
- **B21 (Low)** — `rustfmt.toml` sets nightly-only options
  (`format_strings`, `format_macro_matchers`, `format_macro_bodies`) that
  stable `cargo fmt --check` silently ignores — the configured style isn't
  actually enforced.

---

## 9. Design inconsistencies & dead code

- **X1 (Medium)** — Three independent "what do we call this session" chains:
  `tab_label_long` (`wrapper.rs:353-364`, tray), the inline chain in
  `push_to_device` (`wrapper.rs:446-460`, device), and
  `compute_session_label` (`hooks.rs:1323-1344`, alerts). The alert variant
  **omits `terminal_title`** (contradicting the documented fallback chain in
  `state.rs:200-203`) and lacks the `↦ ` remote prefix the other two add.
  Extract one shared function.
- **X2 (Low)** — `wrapper.rs:1133-1140`: `write_to_session` is genuinely
  dead; `write_to_wrapper` (`:1119-1129`) carries a stale
  `#[allow(dead_code)]` despite being live.
- **X3 (Low)** — `hid/protocol.rs:263-293`: `ProtoError` is entirely dead —
  firmware `Error` (0xFF) responses are accepted but never mapped (pairs
  with H7).
- **X4 (Low)** — `hid/commands.rs:6`: module-wide `#![allow(dead_code)]`
  hides any future orphaned builder; appears removable today.
- **X5 (Low)** — `handle_statusline` returns 200 with an empty body
  (`hooks.rs`), so installing CoreDeck blanks Claude Code's terminal status
  line rather than rendering anything. If intentional, document it.
- **X6 (Note)** — `wrapper_state.json` is never pruned
  (`wrapper_state.rs:13-16`); local wrappers use a fresh UUID per run, so
  entries accumulate forever. Acknowledged in-comment as a trade-off —
  flagged for visibility.
- **X7 (Note)** — `hotplug_macos.rs:162-165` `Box::leak`s the callback
  contexts; fine for the once-per-process watcher, but a future
  "restart watcher" path would leak per restart.

---

## 10. Tests

- **T1 (High, verified)** — `tests/integration/` is dead code: the workspace
  root is a virtual manifest (no `[package]`), so the root `tests/` dir is
  attached to nothing — `cargo test --workspace --no-run` builds exactly
  three unittest binaries and never touches it. The tests also couldn't
  compile: `parser_tests.rs:3` imports `core_deck::pty::{AnsiParser,
  ParsedElement}` — a crate and module that no longer exist (GUI-era
  remnant). `tests/fixtures/ansi_samples/` is likewise orphaned. Delete, or
  port to real targets.
- **T2 (Medium)** — Coverage is concentrated where it's least needed.
  Tested: protocol serde round-trips (11), HID framing (16 + 12), keymap
  (7), updates (5), raise (3). **Zero tests:** `hooks.rs` (2,204 lines —
  hook parsing, settings.json merge/uninstall, enrollment/queue logic),
  `alerts.rs` (the queue/oneshot state machine where C2–C4 live), `ws.rs`,
  `rpc.rs`, `state.rs`, `presets.rs`, `spawn.rs`, and the entire wrapper
  crate — whose OSC title sniffer and focus-event stripper are stateful,
  buffer-spanning parsers that are ideal unit-test targets.

---

## Verified clean (for the record)

- Lock ordering is consistent (`wrappers → claude`, device/hid as leaves);
  no `std::sync::Mutex` held across `.await`; the YOLO opt-in/out sets are
  taken in opposite orders but never nested.
- Parked permission oneshots are bounded (1 active + 4 queued) with a 300 s
  outer timeout — no unbounded task/sender leak.
- `keymap.rs` and the `truncate_*` helpers are char-boundary-safe (C1 is the
  one byte-slice exception found).
- `git ls-files dist/` is empty — no committed build artifacts; `.gitignore`
  covers `dist/` and `.DS_Store`.
- Wrapper↔daemon message coverage is symmetric (modulo the dead `Goodbye`,
  P3); the cached-`SessionBound` daemon-restart re-binding path is
  internally consistent.
- Standalone/VIAL chunking math checks out (VIAL's 29-byte payload cap means
  the dropped 32nd byte is always zero); START/END flag handling is covered
  by tests.
- Release tarball contents, udev rule VID/PID, launchd label, systemd unit,
  port 19384, entitlements hardening claim, and all README CLI commands
  match the code.

## Suggested priority order

1. **B1 + B2** — the release pipeline breaks are cheap fixes (add the `env:`
   block / drop the `--apple-id` flag) and block every signed release.
2. **H1, H2** — Linux device correctness (one-line report-ID fix; udev
   property matching).
3. **S1–S3** — Origin validation + drop `CorsLayer(Any)`; consider a token.
4. **C1** (one-line panic fix), **W1** (signal/panic terminal restore),
   **W2** (connection-token unregister).
5. **B6** (`--workspace` in CI) and **T1** (delete dead tests) — then the
   doc batch: D1/D6/D10/D16–D18 first, the rest mechanically.

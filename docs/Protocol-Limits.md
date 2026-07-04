# Protocol Limits & Constraints

Hard limits enforced by the firmware and daemon that API consumers must respect.

## Text Fields

| Limit | Value | Applies To |
|-------|-------|------------|
| Max text length | **128 bytes** | `session`, `task`, `task2` in display updates; `session`, `text`, `details` in alerts |

Text fields are stored as C strings on the firmware. Values exceeding 128 bytes (including null terminator) will be truncated.

## Tabs

| Limit | Value |
|-------|-------|
| Max tabs | **16** |
| Tab state values | `0` = inactive, `1` = started, `2` = working |

The `tabs` array in display updates can contain at most 16 entries. The `active` index must be within the array bounds.

Per-tab alerts also use tab indices 0–15.

## Display

| Property | Value |
|----------|-------|
| Display resolution | **284 x 76** pixels |

## Brightness

| Limit | Value |
|-------|-------|
| Range | **0–255** |
| Default | 178 (`DISPLAY_BL_DEFAULT_LEVEL` — reduced for the high-VLT cover) |
| Dimmed level | 125 (`DISPLAY_BL_DIM_LEVEL`) |

## Device Modes

Mirror Claude Code's cyclable terminal permission modes. The value is the
low 2 bits of the state byte, so all four fit without a wire change.

| Value | Name | Claude Code mode | Mode-button LED |
|-------|------|------------------|-----------------|
| 0 | Default | `default` | none (reactive glow) |
| 1 | Accept | `acceptEdits` | purple |
| 2 | Plan | `plan` | cyan |
| 3 | Auto | `auto` | golden/amber |

`bypassPermissions` (and any unknown mode) maps to Default. Auto's LED is
a steady solid gold — distinct from Default, whose LED has no override
and shows the normal reactive effect.

Cycle order on the physical mode button: Default → Accept → Plan → Auto →
Default. (The button tap is optimistic; the authoritative mode is whatever
Claude Code reports back through the hooks, which corrects the LED within a
tick.)

## Soft Keys

| Limit | Value |
|-------|-------|
| Number of keys | **3** (indices 0, 1, 2) |
| Max data per key | **128 bytes** |
| Max keycodes per sequence | **63** |

### Soft Key Types

| Value | Type | Data Format |
|-------|------|-------------|
| 0 | Default | Use keymap default (no data) |
| 1 | Keycode | 2 bytes: `[hi, lo]` (16-bit QMK keycode) |
| 2 | String | `[flags, string_bytes...]` — flag bit 0: send Enter after typing |
| 3 | Sequence | `[count, hi0, lo0, hi1, lo1, ...]` — sequence of 16-bit keycodes |

## JSON Payload Size

| Limit | Value |
|-------|-------|
| Max JSON payload | **512 bytes** |

The firmware reassembly buffer (`PROTO_REASSEMBLY_SIZE`) is 512 bytes. JSON payloads sent via `UpdateDisplay` or `Alert` commands must fit within this limit. The HID chunked protocol splits larger host payloads into 30-byte chunks (32-byte HID report minus 2-byte header) and reassembles them on the device. In VIAL mode every packet additionally carries a `0x80` prefix byte, so chunks shrink to 29 payload bytes.

## Timeouts

| Timeout | Value | Behavior |
|---------|-------|----------|
| Ping timeout | **30 seconds** | Firmware dims the display backlight after 30s without any host communication (ping, data, or command) |
| Idle timeout | **15 minutes** | Firmware dims the display after 15 minutes without content changes (display updates or alerts) |

Both timeouts dim the display to the dimmed brightness level (125/255). Any new communication or user activity (key press) restores the configured brightness.

## WebSocket Sequence Numbers

| Limit | Value |
|-------|-------|
| Range | **u16 (0–65535)** |
| Reserved | `0` is reserved for events; commands must use `seq > 0` |

Clients should use a monotonically incrementing counter for command sequence numbers, wrapping from 65535 back to 1 (skipping 0). The daemon enforces this: a command frame with `seq == 0` is rejected with a `CommandError`.

## WebSocket Exclusivity

Only one WebSocket client may be connected at a time. Attempting a second connection returns HTTP 409 on the WebSocket upgrade request. While a client holds the lock, all mutating HTTP REST endpoints return 409, as do the GET endpoints that talk to the device (`GET /api/version`, `GET /api/soft-keys`, `GET /api/theme`).

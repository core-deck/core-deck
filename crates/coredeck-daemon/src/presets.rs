//! Soft-key presets — built-in collections + user-defined storage.
//!
//! Each preset names three soft-key configurations in the daemon's wire
//! format (`SoftKeyType` + raw bytes). Built-ins are hardcoded; user
//! presets are persisted as TOML in the platform data directory and
//! upserted via the settings page.
//!
//! Wire format reference:
//! - `Keycode`: `[kc_hi, kc_lo]` — 16-bit QMK keycode (modifiers in upper byte).
//! - `String`: `[flags, ...utf8 bytes]` — flags bit 0 = send Enter after.
//!   No trailing null — firmware appends one on `set`, and the `get`
//!   path returns bytes excluding it.
//! - `Sequence`: `[count, kc_hi_0, kc_lo_0, ..., kc_hi_{N-1}, kc_lo_{N-1}]`.

use coredeck_protocol::SoftKeyType;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::warn;

/// One key inside a preset, in wire format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetKey {
    pub key_type: SoftKeyType,
    #[serde(default)]
    pub data: Vec<u8>,
}

/// A named bundle of three soft-key configs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub keys: [PresetKey; 3],
}

// ── QMK keycode constants used by the built-in presets ────────────
//
// Documented inline so the preset table reads at-a-glance without
// crossing to a constants file. Modifiers occupy bits 8-12; for
// example `LCTL(KC_O)` is `0x0100 | 0x0012 = 0x0112`.
const KC_ESC: u16 = 0x0029;
const KC_O: u16 = 0x0012;
const KC_G: u16 = 0x000A;
const MOD_LCTL: u16 = 0x0100;

const FLAG_SEND_ENTER: u8 = 0x01;

/// Build a Keycode preset key from a 16-bit QMK keycode.
fn key_keycode(kc: u16) -> PresetKey {
    PresetKey {
        key_type: SoftKeyType::Keycode,
        data: vec![(kc >> 8) as u8, (kc & 0xFF) as u8],
    }
}

/// Build a String preset key. The firmware's `set` path appends its own
/// null terminator, and the `get` path returns the string without one
/// (`1 + strlen(...)`), so we omit the null here — byte-equal to what
/// `GET /api/soft-keys` reports, which matters for preset matching.
fn key_string(text: &str, send_enter: bool) -> PresetKey {
    let flags = if send_enter { FLAG_SEND_ENTER } else { 0 };
    let mut data = vec![flags];
    let mut bytes = text.as_bytes().to_vec();
    bytes.truncate(126);
    data.extend_from_slice(&bytes);
    PresetKey { key_type: SoftKeyType::String, data }
}

/// Build a Sequence preset key from a list of (already-composed)
/// 16-bit keycodes. Capped at 63 entries to fit firmware limits.
fn key_sequence(codes: &[u16]) -> PresetKey {
    let count = codes.len().min(63) as u8;
    let mut data = vec![count];
    for kc in codes.iter().take(63) {
        data.push((kc >> 8) as u8);
        data.push((kc & 0xFF) as u8);
    }
    PresetKey { key_type: SoftKeyType::Sequence, data }
}

/// Names of presets that ship with the binary; user presets are
/// rejected if they collide with one of these (case-insensitive).
pub const BUILTIN_PRESET_NAMES: &[&str] = &["Default", "Vim", "Git", "Context", "GSD"];

pub fn is_builtin_name(name: &str) -> bool {
    BUILTIN_PRESET_NAMES
        .iter()
        .any(|b| b.eq_ignore_ascii_case(name))
}

/// The five presets carried over from the GUI's settings modal —
/// translated into wire bytes once at compile time.
pub fn builtin_presets() -> Vec<Preset> {
    vec![
        Preset {
            name: "Default".into(),
            description: Some(
                "Esc+Esc (clear input), Ctrl+O (verbose), /model (models)".into(),
            ),
            keys: [
                key_sequence(&[KC_ESC, KC_ESC]),
                key_keycode(MOD_LCTL | KC_O),
                key_string("/model", true),
            ],
        },
        Preset {
            name: "Vim".into(),
            description: Some("Ctrl+G (vim mode), :w (save), :q (quit)".into()),
            keys: [
                key_keycode(MOD_LCTL | KC_G),
                key_string(":w", true),
                key_string(":q", true),
            ],
        },
        Preset {
            name: "Git".into(),
            description: Some("/commit, /pr, Esc+Esc (rewind)".into()),
            keys: [
                key_string("/commit", true),
                key_string("/pr", true),
                key_sequence(&[KC_ESC, KC_ESC]),
            ],
        },
        Preset {
            name: "Context".into(),
            description: Some("/compact, /clear, /memory".into()),
            keys: [
                key_string("/compact", true),
                key_string("/clear", true),
                key_string("/memory", true),
            ],
        },
        Preset {
            name: "GSD".into(),
            description: Some("discuss-phase, plan-phase, execute-phase".into()),
            keys: [
                key_string("/gsd:discuss-phase ", false),
                key_string("/gsd:plan-phase ", false),
                key_string("/gsd:execute-phase ", false),
            ],
        },
    ]
}

// ── User preset persistence ────────────────────────────────────────

/// On-disk shape, kept simple so the file is reasonable to hand-edit.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct UserPresetFile {
    #[serde(default)]
    presets: Vec<Preset>,
}

/// In-memory store of user-defined presets. Persisted to TOML in the
/// platform data directory; loaded once at daemon start (and re-loaded
/// on demand by the REST handlers — each list/save call goes through
/// `load`/`save` so the file is the source of truth even if multiple
/// daemons or external editors touch it).
#[derive(Debug, Clone, Default)]
pub struct UserPresetStore {
    pub presets: Vec<Preset>,
}

impl UserPresetStore {
    /// Load from disk; returns an empty store if the file is missing
    /// or unparseable. Errors are logged but never propagated — preset
    /// management is non-critical.
    pub fn load() -> Self {
        let Some(path) = data_path() else {
            return Self::default();
        };
        if !path.exists() {
            return Self::default();
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, path = ?path, "preset file unreadable");
                return Self::default();
            }
        };
        match toml::from_str::<UserPresetFile>(&content) {
            Ok(file) => Self { presets: file.presets },
            Err(e) => {
                warn!(error = %e, path = ?path, "preset file unparseable");
                Self::default()
            }
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = data_path().ok_or_else(|| "no data dir available".to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {}", parent.display(), e))?;
        }
        let file = UserPresetFile { presets: self.presets.clone() };
        let content =
            toml::to_string_pretty(&file).map_err(|e| format!("serialize: {}", e))?;
        std::fs::write(&path, content).map_err(|e| format!("write: {}", e))
    }

    /// Insert or replace by name (case-sensitive — user preset names
    /// are user-chosen and we honour their casing).
    pub fn upsert(&mut self, preset: Preset) {
        if let Some(slot) = self.presets.iter_mut().find(|p| p.name == preset.name) {
            *slot = preset;
        } else {
            self.presets.push(preset);
        }
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.presets.len();
        self.presets.retain(|p| p.name != name);
        self.presets.len() < before
    }
}

fn data_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "coredeck", "CoreDeck")?;
    Some(dirs.data_dir().join("soft_key_presets.toml"))
}

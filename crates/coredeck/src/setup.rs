//! First-run setup beyond the hooks: the command-line tools
//! (`coredeck`, `coredeck-claude`) and start-at-login (launchd / systemd).
//!
//! Homebrew's cask and the Linux `install.sh` put the tools on PATH, but a
//! drag-install of the DMG doesn't — and without `coredeck-claude` there
//! is no wrapper, so the deck shows sessions while its keys reach none of
//! them. The settings page drives these through `/api/setup`; the
//! `coredeck setup` / `install` / `uninstall` CLI shares the same code.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::hooks;

/// The binaries the command-line tools consist of.
const TOOLS: [&str; 2] = ["coredeck", "coredeck-claude"];
const WRAPPER: &str = "coredeck-claude";
/// Directories on the default PATH where `coredeck-claude` needs no alias
/// path (Homebrew's bin dirs, the system one).
const PATH_DIRS: [&str; 3] = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"];

#[derive(Debug, Serialize)]
pub struct SetupStatus {
    pub hooks: bool,
    pub cli: CliStatus,
    pub alias: AliasStatus,
    pub autostart: AutostartStatus,
    /// Hooks, command-line tools and start-at-login are all in place. The
    /// alias only counts as informational — it's found heuristically.
    pub complete: bool,
}

#[derive(Debug, Serialize)]
pub struct CliStatus {
    /// `"linked"` (our symlinks, removable), `"installed"` (provided by
    /// Homebrew or install.sh), or `"missing"`.
    pub state: &'static str,
    /// Where `coredeck-claude` was found.
    pub path: Option<String>,
    /// Where `install_cli` links the tools.
    pub link_dir: String,
}

#[derive(Debug, Serialize)]
pub struct AliasStatus {
    /// The line to add to the shell's rc file.
    pub line: String,
    /// The rc file to add it to (or the one that already mentions the
    /// wrapper).
    pub rc_file: String,
    /// True when `rc_file` already mentions `coredeck-claude`.
    pub found: bool,
}

#[derive(Debug, Serialize)]
pub struct AutostartStatus {
    pub installed: bool,
    pub path: String,
}

pub fn status() -> SetupStatus {
    let hooks = hooks::are_hooks_installed();
    let cli = cli_status();
    let alias = alias_status(&cli);
    let autostart = AutostartStatus {
        installed: autostart_path().exists(),
        path: tilde(&autostart_path()),
    };
    let complete = hooks && cli.state != "missing" && autostart.installed;
    SetupStatus {
        hooks,
        cli,
        alias,
        autostart,
        complete,
    }
}

// ── Command-line tools ─────────────────────────────────────────────

fn link_dir() -> PathBuf {
    hooks::home_dir().join(".local/bin")
}

/// Directory holding the running daemon's binary (inside `Core Deck.app`
/// on macOS), with symlinks such as Homebrew's resolved.
fn exe_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    exe.parent().map(Path::to_path_buf)
}

fn is_symlink(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

fn cli_status() -> CliStatus {
    let link_dir = link_dir();
    let found = std::iter::once(link_dir.clone())
        .chain(PATH_DIRS.iter().map(PathBuf::from))
        .map(|d| d.join(WRAPPER))
        .find(|p| p.exists());
    let state = match &found {
        None => "missing",
        Some(p) if p.parent() == Some(link_dir.as_path()) && is_symlink(p) => "linked",
        Some(_) => "installed",
    };
    CliStatus {
        state,
        path: found.as_deref().map(tilde),
        link_dir: tilde(&link_dir),
    }
}

/// Symlink `coredeck` and `coredeck-claude` into `~/.local/bin`, pointing
/// at the binaries next to the running daemon. Replaces stale links (e.g.
/// after the app moved); never overwrites a real file.
#[cfg(unix)]
pub fn install_cli() -> Result<String, String> {
    let src_dir = exe_dir().ok_or("can't locate the CoreDeck binaries")?;
    link_tools(&src_dir, &link_dir())
}

/// Remove the links `install_cli` made. Real files (install.sh) are left.
#[cfg(unix)]
pub fn uninstall_cli() -> Result<String, String> {
    unlink_tools(&link_dir())
}

#[cfg(unix)]
fn link_tools(src_dir: &Path, dir: &Path) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    for tool in TOOLS {
        let src = src_dir.join(tool);
        if !src.exists() {
            return Err(format!("{} not found next to the daemon", src.display()));
        }
        let link = dir.join(tool);
        if is_symlink(&link) {
            std::fs::remove_file(&link)
                .map_err(|e| format!("replacing {}: {e}", link.display()))?;
        } else if link.exists() {
            return Err(format!(
                "{} already exists and isn't a link — leaving it",
                link.display()
            ));
        }
        std::os::unix::fs::symlink(&src, &link)
            .map_err(|e| format!("linking {}: {e}", link.display()))?;
    }
    Ok(format!(
        "Linked coredeck and coredeck-claude into {}",
        tilde(dir)
    ))
}

#[cfg(unix)]
fn unlink_tools(dir: &Path) -> Result<String, String> {
    for tool in TOOLS {
        let link = dir.join(tool);
        if is_symlink(&link) {
            std::fs::remove_file(&link).map_err(|e| format!("removing {}: {e}", link.display()))?;
        }
    }
    Ok(format!("Removed the links from {}", tilde(dir)))
}

#[cfg(not(unix))]
pub fn install_cli() -> Result<String, String> {
    Err("linking the command-line tools is only supported on macOS and Linux".into())
}

#[cfg(not(unix))]
pub fn uninstall_cli() -> Result<String, String> {
    install_cli()
}

// ── Shell alias ────────────────────────────────────────────────────

/// Shell rc files checked for an existing `coredeck-claude` alias.
const RC_FILES: [&str; 5] = [
    ".zshrc",
    ".bashrc",
    ".bash_profile",
    ".profile",
    ".config/fish/config.fish",
];

fn alias_status(cli: &CliStatus) -> AliasStatus {
    // On the default PATH the bare name works; otherwise spell out the
    // path so ~/.local/bin needn't be on PATH.
    let wrapper = match cli.path.as_deref() {
        Some(p) if PATH_DIRS.iter().any(|d| p.starts_with(d)) => WRAPPER.to_string(),
        Some(p) => p.replacen('~', "$HOME", 1),
        None => format!("$HOME/.local/bin/{WRAPPER}"),
    };
    let home = hooks::home_dir();
    let found_in = RC_FILES
        .iter()
        .find(|rc| std::fs::read_to_string(home.join(rc)).is_ok_and(|s| s.contains(WRAPPER)));
    let default_rc = if cfg!(target_os = "macos") {
        ".zshrc"
    } else {
        ".bashrc"
    };
    AliasStatus {
        line: format!("alias claude=\"{wrapper}\""),
        rc_file: format!("~/{}", found_in.copied().unwrap_or(default_rc)),
        found: found_in.is_some(),
    }
}

// ── Start at login ─────────────────────────────────────────────────

const LAUNCHD_LABEL: &str = "com.coredeck.daemon";

fn autostart_path() -> PathBuf {
    let home = hooks::home_dir();
    if cfg!(target_os = "linux") {
        home.join(".config/systemd/user/coredeck.service")
    } else {
        home.join(format!("Library/LaunchAgents/{LAUNCHD_LABEL}.plist"))
    }
}

/// Install the start-at-login agent (launchd plist / systemd user unit)
/// running this binary with `--listen <listen>`.
///
/// `restart`: the CLI reloads / restarts the job so an upgrade takes
/// effect. The settings page passes false — the daemon serving it may be
/// that very job, and unloading it would kill the process before it could
/// load the job again, leaving nothing running. Without a restart the job
/// is loaded; if it starts a second daemon now, that one sees the port
/// taken and exits cleanly, and the agent takes over from the next login.
pub fn install_autostart(listen: &str, restart: bool) -> Result<String, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("locating the daemon binary: {e}"))?
        .to_string_lossy()
        .to_string();
    let path = autostart_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    install_autostart_platform(&path, &exe, listen, restart)
}

#[cfg(target_os = "macos")]
fn install_autostart_platform(
    path: &Path,
    exe: &str,
    listen: &str,
    restart: bool,
) -> Result<String, String> {
    let home = hooks::home_dir();
    let home = home.to_string_lossy();
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>--listen</string>
        <string>{listen}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>StandardOutPath</key>
    <string>{home}/Library/Logs/coredeck.log</string>
    <key>StandardErrorPath</key>
    <string>{home}/Library/Logs/coredeck.log</string>
</dict>
</plist>"#,
        exe = xml_escape(exe),
        listen = xml_escape(listen),
        home = xml_escape(&home),
    );
    std::fs::write(path, plist).map_err(|e| format!("writing {}: {e}", path.display()))?;
    let path_str = path.to_string_lossy();
    if restart {
        // Idempotent reload: unload silently first (no-op if not loaded).
        let _ = std::process::Command::new("launchctl")
            .args(["unload", &path_str])
            .stderr(std::process::Stdio::null())
            .status();
    }
    let status = std::process::Command::new("launchctl")
        .args(["load", &path_str])
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("running launchctl: {e}"))?;
    // Without the unload, `load` of an already-loaded job fails harmlessly.
    if status.success() || !restart {
        Ok(format!("Installed {}", tilde(path)))
    } else {
        Err(format!(
            "launchctl load failed (exit {})",
            status.code().unwrap_or(-1)
        ))
    }
}

/// systemd user unit — no root needed; journald keeps the logs.
/// `Restart=on-failure` mirrors launchd's crash-only KeepAlive, and
/// `WantedBy=default.target` starts it with the user session.
#[cfg(target_os = "linux")]
fn install_autostart_platform(
    path: &Path,
    exe: &str,
    listen: &str,
    restart: bool,
) -> Result<String, String> {
    let unit = format!(
        r#"[Unit]
Description=CoreDeck daemon
After=graphical-session.target
PartOf=graphical-session.target

[Service]
Type=simple
ExecStart="{exe}" --listen {listen}
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
"#
    );
    std::fs::write(path, unit).map_err(|e| format!("writing {}: {e}", path.display()))?;
    let systemctl = |args: &[&str]| {
        std::process::Command::new("systemctl")
            .arg("--user")
            .args(args)
            .stderr(std::process::Stdio::null())
            .status()
    };
    let _ = systemctl(&["daemon-reload"]);
    // `--now` starts it immediately; when this daemon isn't the unit, the
    // started one sees the port taken and exits cleanly.
    let enable = if restart {
        systemctl(&["enable", "--now", "coredeck.service"])
    } else {
        systemctl(&["enable", "coredeck.service"])
    };
    if restart {
        let _ = systemctl(&["restart", "coredeck.service"]);
    }
    match enable {
        Ok(s) if s.success() => Ok(format!("Installed {}", tilde(path))),
        Ok(s) => Err(format!(
            "systemctl --user enable failed (exit {})",
            s.code().unwrap_or(-1)
        )),
        Err(e) => Err(format!("running systemctl: {e}")),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn install_autostart_platform(
    _path: &Path,
    _exe: &str,
    _listen: &str,
    _restart: bool,
) -> Result<String, String> {
    Err("start-at-login is only supported on macOS (launchd) and Linux (systemd)".into())
}

/// Remove the start-at-login agent. `stop`: the CLI also stops the job
/// now; the settings page doesn't (see `install_autostart`), so the
/// change takes effect at the next login.
pub fn uninstall_autostart(stop: bool) -> Result<String, String> {
    let path = autostart_path();
    #[cfg(target_os = "macos")]
    if stop && path.exists() {
        let _ = std::process::Command::new("launchctl")
            .args(["unload", &path.to_string_lossy()])
            .status();
    }
    #[cfg(target_os = "linux")]
    {
        let args: &[&str] = if stop {
            &["--user", "disable", "--now", "coredeck.service"]
        } else {
            &["--user", "disable", "coredeck.service"]
        };
        let _ = std::process::Command::new("systemctl")
            .args(args)
            .stderr(std::process::Stdio::null())
            .status();
    }
    if !path.exists() {
        return Ok(format!("Not installed: {}", tilde(&path)));
    }
    std::fs::remove_file(&path).map_err(|e| format!("removing {}: {e}", path.display()))?;
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    Ok(format!("Removed {}", tilde(&path)))
}

// ── Helpers ────────────────────────────────────────────────────────

/// `path` with the home directory shown as `~`.
fn tilde(path: &Path) -> String {
    let home = hooks::home_dir();
    match path.strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(any(target_os = "macos", test))]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_uses_bare_name_only_on_the_default_path() {
        let cli = |path: Option<&str>| CliStatus {
            state: "installed",
            path: path.map(str::to_string),
            link_dir: "~/.local/bin".into(),
        };
        assert_eq!(
            alias_status(&cli(Some("/opt/homebrew/bin/coredeck-claude"))).line,
            r#"alias claude="coredeck-claude""#
        );
        assert_eq!(
            alias_status(&cli(Some("~/.local/bin/coredeck-claude"))).line,
            r#"alias claude="$HOME/.local/bin/coredeck-claude""#
        );
        assert_eq!(
            alias_status(&cli(None)).line,
            r#"alias claude="$HOME/.local/bin/coredeck-claude""#
        );
    }

    #[cfg(unix)]
    #[test]
    fn links_tools_replaces_stale_links_and_keeps_real_files() {
        let tmp = std::env::temp_dir().join(format!("coredeck-setup-test-{}", std::process::id()));
        let (src, bin) = (tmp.join("app"), tmp.join("bin"));
        std::fs::create_dir_all(&src).unwrap();
        for tool in TOOLS {
            std::fs::write(src.join(tool), "#!/bin/sh\n").unwrap();
        }
        // A stale link (app moved) is replaced.
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(tmp.join("gone/coredeck"), bin.join("coredeck")).unwrap();
        link_tools(&src, &bin).unwrap();
        for tool in TOOLS {
            assert_eq!(std::fs::read_link(bin.join(tool)).unwrap(), src.join(tool));
        }
        unlink_tools(&bin).unwrap();
        assert!(!bin.join(WRAPPER).exists());
        // A real file (e.g. from install.sh) is never overwritten.
        std::fs::write(bin.join(WRAPPER), "real").unwrap();
        assert!(link_tools(&src, &bin).is_err());
        unlink_tools(&bin).unwrap();
        assert_eq!(std::fs::read_to_string(bin.join(WRAPPER)).unwrap(), "real");
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn xml_escape_handles_paths_with_markup() {
        assert_eq!(
            xml_escape(r#"/a & <b> "c""#),
            "/a &amp; &lt;b&gt; &quot;c&quot;"
        );
    }
}

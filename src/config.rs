//! `/etc/garde/config.toml` (written by the Nix module) and the small state file that remembers
//! the last session picked at the login screen.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_CONFIG: &str = "/etc/garde/config.toml";
pub const DEFAULT_STATE: &str = "/var/lib/garde/state.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The only user. Shown on the card, never typed.
    pub user: String,
    /// Image drawn behind everything, scaled to fill each output.
    pub wallpaper: Option<PathBuf>,
    /// xkb layout codes, in the order the compositor's keymap has them. The first is the default.
    pub layouts: Vec<String>,
    /// Turn num lock on at start.
    pub numlock: bool,
    /// Darkening of the wallpaper behind the card, 0 = none, 1 = black.
    pub dim: f32,
    /// Blur radius (logical px) of the wallpaper behind the card, 0 = off.
    pub blur: f32,
    /// Font family for all text.
    pub font: String,
    /// PAM service used by `garde lock`.
    pub pam_service: String,
    /// Where `garde login` keeps the last session picked.
    pub state_file: PathBuf,
    /// Command prepended to X11 session commands (greetd starts no X server itself).
    pub xsession_wrapper: Vec<String>,
    pub sessions: SessionsConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionsConfig {
    pub dirs: Vec<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            user: String::new(),
            wallpaper: None,
            layouts: vec!["us".into()],
            numlock: false,
            dim: 0.35,
            blur: 0.0,
            font: "Noto Sans".into(),
            pam_service: "garde".into(),
            state_file: DEFAULT_STATE.into(),
            xsession_wrapper: vec!["startx".into(), "/usr/bin/env".into()],
            sessions: SessionsConfig::default(),
        }
    }
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            dirs: vec![
                "/run/current-system/sw/share/wayland-sessions".into(),
                "/run/current-system/sw/share/xsessions".into(),
            ],
        }
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut cfg: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        cfg.dim = cfg.dim.clamp(0.0, 1.0);
        cfg.blur = cfg.blur.clamp(0.0, 200.0);
        if cfg.layouts.is_empty() {
            cfg.layouts.push("us".into());
        }
        Ok(cfg)
    }

    /// Load the config. A missing file is an error only if no user can be found: falling back to
    /// the user running garde keeps `garde lock` usable without the Nix module.
    pub fn load(path: &Path) -> Result<Self, String> {
        let mut cfg = match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::warn!("{} not found, using defaults", path.display());
                Config::default()
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        if cfg.user.is_empty() {
            cfg.user = crate::user::current_login().ok_or("no `user` in config and no current user")?;
        }
        Ok(cfg)
    }
}

/// Persistent login-screen state. Only the last session picked.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct State {
    /// Desktop-file id (file name without `.desktop`) of the last session started.
    pub last_session: Option<String>,
}

impl State {
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                log::warn!("ignoring unreadable state {}: {e}", path.display());
                State::default()
            }),
            Err(_) => State::default(),
        }
    }

    /// Write atomically (temp file + rename) so a crash never leaves a truncated file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = toml::to_string(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_example() {
        let cfg = Config::parse(
            r#"
            user = "sigma"
            wallpaper = "/nix/store/x-wallpaper.png"
            layouts = ["fr", "us"]
            numlock = true
            dim = 0.35
            blur = 0
            font = "Noto Sans"
            [sessions]
            dirs = ["/a", "/b"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.user, "sigma");
        assert_eq!(cfg.layouts, ["fr", "us"]);
        assert!(cfg.numlock);
        assert_eq!(cfg.blur, 0.0);
        assert_eq!(cfg.sessions.dirs, [PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(cfg.pam_service, "garde");
    }

    #[test]
    fn rejects_unknown_keys_and_clamps() {
        assert!(Config::parse("usr = \"x\"").is_err());
        let cfg = Config::parse("dim = 4.0\nlayouts = []").unwrap();
        assert_eq!(cfg.dim, 1.0);
        assert_eq!(cfg.layouts, ["us"]);
    }

    #[test]
    fn state_round_trips_and_tolerates_garbage() {
        let dir = std::env::temp_dir().join(format!("garde-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.toml");

        assert_eq!(State::load(&path), State::default());
        let s = State { last_session: Some("cosmic".into()) };
        s.save(&path).unwrap();
        assert_eq!(State::load(&path), s);
        assert!(!path.with_extension("toml.tmp").exists());

        std::fs::write(&path, "last_session = [").unwrap();
        assert_eq!(State::load(&path), State::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

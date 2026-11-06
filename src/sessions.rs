//! Session discovery from `wayland-sessions` / `xsessions` desktop files.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Wayland,
    X11,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// File name without `.desktop`; what the state file remembers and XDG_SESSION_DESKTOP.
    pub id: String,
    pub name: String,
    pub exec: Vec<String>,
    pub desktop_names: Vec<String>,
    pub kind: Kind,
}

impl Session {
    /// `cmd` and `env` for greetd's StartSession.
    pub fn launch(&self, xsession_wrapper: &[String]) -> (Vec<String>, Vec<String>) {
        let mut cmd = Vec::new();
        if self.kind == Kind::X11 {
            cmd.extend(xsession_wrapper.iter().cloned());
        }
        cmd.extend(self.exec.iter().cloned());
        let mut env = vec![
            format!("XDG_SESSION_TYPE={}", if self.kind == Kind::X11 { "x11" } else { "wayland" }),
            format!("XDG_SESSION_DESKTOP={}", self.id),
        ];
        if !self.desktop_names.is_empty() {
            env.push(format!("XDG_CURRENT_DESKTOP={}", self.desktop_names.join(":")));
        }
        (cmd, env)
    }
}

/// Every visible session in `dirs`, in directory order then file-name order. A session id found
/// in an earlier directory hides the same id in later ones (as XDG data dirs do).
pub fn discover(dirs: &[PathBuf]) -> Vec<Session> {
    let mut out: Vec<Session> = Vec::new();
    for dir in dirs {
        let kind = kind_of_dir(dir);
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        let mut files: Vec<PathBuf> =
            rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "desktop")).collect();
        files.sort();
        for path in files {
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            if out.iter().any(|s| s.id == id) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            match parse_desktop(id, &text, kind) {
                Some(s) if try_exec_ok(&text) => out.push(s),
                _ => log::debug!("skipping session {}", path.display()),
            }
        }
    }
    out
}

fn kind_of_dir(dir: &Path) -> Kind {
    if dir.file_name().is_some_and(|n| n == "xsessions") {
        Kind::X11
    } else {
        Kind::Wayland
    }
}

/// Key/value pairs of the `[Desktop Entry]` group, unlocalized keys only.
fn entry_keys(text: &str) -> impl Iterator<Item = (&str, &str)> {
    let mut in_entry = false;
    text.lines().filter_map(move |line| {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            return None;
        }
        if !in_entry || line.starts_with('#') {
            return None;
        }
        let (k, v) = line.split_once('=')?;
        Some((k.trim(), v.trim()))
    })
}

fn get<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    entry_keys(text).find(|(k, _)| *k == key).map(|(_, v)| v)
}

fn is_true(v: Option<&str>) -> bool {
    v.is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

/// Parse one session file. `None` for hidden, non-application or broken entries.
pub fn parse_desktop(id: &str, text: &str, kind: Kind) -> Option<Session> {
    if is_true(get(text, "Hidden")) || is_true(get(text, "NoDisplay")) {
        return None;
    }
    // Session files use Application or the older XSession.
    if get(text, "Type").is_some_and(|t| t != "Application" && t != "XSession") {
        return None;
    }
    let exec = split_exec(&unescape(get(text, "Exec")?))?;
    if exec.is_empty() {
        return None;
    }
    let name = get(text, "Name").map(unescape).filter(|n| !n.is_empty()).unwrap_or_else(|| id.to_string());
    let desktop_names = get(text, "DesktopNames")
        .map(|v| v.split(';').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect())
        .unwrap_or_default();
    Some(Session { id: id.to_string(), name, exec, desktop_names, kind })
}

fn try_exec_ok(text: &str) -> bool {
    let Some(t) = get(text, "TryExec").map(unescape) else { return true };
    let p = Path::new(&t);
    if p.is_absolute() {
        return p.exists();
    }
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(&t).exists()))
}

/// Value-level escapes of the desktop entry spec (`\s`, `\n`, `\t`, `\r`, `\\`).
fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut it = v.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Split an `Exec` value into argv: double-quoted arguments with `\"` `\`` `\$` `\\` escapes,
/// field codes (`%f`, `%U`, …) dropped and `%%` turned into `%`. `None` on unbalanced quotes.
pub fn split_exec(exec: &str) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut has_arg = false;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                has_arg = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            e @ ('"' | '`' | '$' | '\\') => cur.push(e),
                            e => {
                                cur.push('\\');
                                cur.push(e);
                            }
                        },
                        o => cur.push(o),
                    }
                }
            }
            ' ' | '\t' => {
                if has_arg {
                    args.push(std::mem::take(&mut cur));
                    has_arg = false;
                }
            }
            // A field code alone is dropped; inside a word it expands to nothing.
            '%' => {
                if let Some('%') = chars.next() {
                    cur.push('%');
                    has_arg = true;
                }
            }
            o => {
                cur.push(o);
                has_arg = true;
            }
        }
    }
    if has_arg {
        args.push(cur);
    }
    Some(args)
}

/// Index of `last` in `sessions`, else 0.
pub fn default_index(sessions: &[Session], last: Option<&str>) -> usize {
    last.and_then(|id| sessions.iter().position(|s| s.id == id)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const COSMIC: &str = "[Desktop Entry]\nName=COSMIC\nComment=This session logs you into the COSMIC desktop\nExec=/nix/store/abc-cosmic-session/bin/start-cosmic\nType=Application\nDesktopNames=COSMIC\n";

    #[test]
    fn parses_cosmic() {
        let s = parse_desktop("cosmic", COSMIC, Kind::Wayland).unwrap();
        assert_eq!(s.name, "COSMIC");
        assert_eq!(s.exec, ["/nix/store/abc-cosmic-session/bin/start-cosmic"]);
        assert_eq!(s.desktop_names, ["COSMIC"]);
        let (cmd, env) = s.launch(&[]);
        assert_eq!(cmd, s.exec);
        assert_eq!(
            env,
            ["XDG_SESSION_TYPE=wayland", "XDG_SESSION_DESKTOP=cosmic", "XDG_CURRENT_DESKTOP=COSMIC"]
        );
    }

    #[test]
    fn plasma_desktop_names_and_x11_wrapper() {
        let text = "[Desktop Entry]\nType=XSession\nExec=startplasma-x11\nName=Plasma (X11)\nDesktopNames=KDE;Plasma;\n";
        assert!(parse_desktop("plasmax11", &text.replace("XSession", "Link"), Kind::X11).is_none());
        let s = parse_desktop("plasmax11", text, Kind::X11).unwrap();
        let (cmd, env) = s.launch(&["startx".into(), "/usr/bin/env".into()]);
        assert_eq!(cmd, ["startx", "/usr/bin/env", "startplasma-x11"]);
        assert!(env.contains(&"XDG_SESSION_TYPE=x11".to_string()));
        assert!(env.contains(&"XDG_CURRENT_DESKTOP=KDE:Plasma".to_string()));
    }

    #[test]
    fn skips_hidden_nodisplay_and_execless() {
        assert!(parse_desktop("a", &format!("{COSMIC}NoDisplay=true\n"), Kind::Wayland).is_none());
        assert!(parse_desktop("a", &format!("{COSMIC}Hidden=True\n"), Kind::Wayland).is_none());
        assert!(parse_desktop("a", "[Desktop Entry]\nName=x\n", Kind::Wayland).is_none());
        // Keys of other groups don't count.
        let t = format!("{COSMIC}[Desktop Action x]\nNoDisplay=true\n");
        assert!(parse_desktop("a", &t, Kind::Wayland).is_some());
        // Name falls back to the id.
        let s = parse_desktop("sway", "[Desktop Entry]\nExec=sway\n", Kind::Wayland).unwrap();
        assert_eq!(s.name, "sway");
    }

    #[test]
    fn exec_quoting_and_field_codes() {
        assert_eq!(split_exec("a b  c").unwrap(), ["a", "b", "c"]);
        assert_eq!(split_exec(r#""/opt/my app/run" --x "y \"z\"" %U"#).unwrap(), ["/opt/my app/run", "--x", "y \"z\""]);
        assert_eq!(split_exec("run 100%% %f").unwrap(), ["run", "100%"]);
        assert_eq!(split_exec(r#"sh -c "echo \$HOME""#).unwrap(), ["sh", "-c", "echo $HOME"]);
        assert_eq!(split_exec(r#""""#).unwrap(), [""]);
        assert!(split_exec(r#"a "b"#).is_none());
        assert_eq!(split_exec(&unescape(r"a\sb")).unwrap(), ["a", "b"]);
    }

    #[test]
    fn discovery_order_dedupe_and_kinds() {
        let root = std::env::temp_dir().join(format!("garde-sessions-{}", std::process::id()));
        let wl = root.join("wayland-sessions");
        let x = root.join("xsessions");
        std::fs::create_dir_all(&wl).unwrap();
        std::fs::create_dir_all(&x).unwrap();
        std::fs::write(wl.join("plasma.desktop"), "[Desktop Entry]\nName=Plasma\nExec=startplasma-wayland\n").unwrap();
        std::fs::write(wl.join("cosmic.desktop"), COSMIC).unwrap();
        std::fs::write(wl.join("hidden.desktop"), format!("{COSMIC}Hidden=true\n")).unwrap();
        std::fs::write(wl.join("missing.desktop"), format!("{COSMIC}TryExec=/nonexistent/garde\n")).unwrap();
        std::fs::write(wl.join("notes.txt"), "x").unwrap();
        std::fs::write(x.join("cosmic.desktop"), "[Desktop Entry]\nName=Dup\nExec=dup\n").unwrap();
        std::fs::write(x.join("i3.desktop"), "[Desktop Entry]\nName=i3\nExec=i3\n").unwrap();

        let found = discover(&[wl.clone(), x.clone(), root.join("absent")]);
        let ids: Vec<_> = found.iter().map(|s| (s.id.as_str(), s.kind)).collect();
        assert_eq!(ids, [("cosmic", Kind::Wayland), ("plasma", Kind::Wayland), ("i3", Kind::X11)]);
        assert_eq!(default_index(&found, Some("i3")), 2);
        assert_eq!(default_index(&found, Some("gone")), 0);
        assert_eq!(default_index(&found, None), 0);
        std::fs::remove_dir_all(&root).unwrap();
    }
}

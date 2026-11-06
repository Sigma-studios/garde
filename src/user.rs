//! Display-name lookup: the GECOS full name, else the login name.

use std::ffi::{CStr, CString};

/// The full-name part of a GECOS field (`Full Name,Room,Work phone,Home phone,Other`).
pub fn gecos_full_name(gecos: &str) -> Option<&str> {
    let name = gecos.split(',').next()?.trim();
    (!name.is_empty()).then_some(name)
}

/// The name shown on the card for `login`, given its GECOS field if the account exists.
pub fn display_name_from(login: &str, gecos: Option<&str>) -> String {
    gecos
        .and_then(gecos_full_name)
        // The classic `&` shorthand stands for the capitalized login name.
        .map(|n| n.replace('&', &capitalize(login)))
        .unwrap_or_else(|| login.to_string())
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// Look `login` up through NSS (so it works for LDAP/systemd-homed users too).
pub fn display_name(login: &str) -> String {
    display_name_from(login, passwd_field(login, |pw| pw.pw_gecos).as_deref())
}

/// Login name of the user running garde.
pub fn current_login() -> Option<String> {
    let uid = unsafe { libc::getuid() };
    with_passwd(|buf, pw, out| unsafe { libc::getpwuid_r(uid, pw, buf.as_mut_ptr(), buf.len(), out) }, |pw| pw.pw_name)
        .or_else(|| std::env::var("USER").ok())
}

fn passwd_field(login: &str, field: impl Fn(&libc::passwd) -> *mut libc::c_char) -> Option<String> {
    let name = CString::new(login).ok()?;
    with_passwd(|buf, pw, out| unsafe { libc::getpwnam_r(name.as_ptr(), pw, buf.as_mut_ptr(), buf.len(), out) }, field)
}

fn with_passwd(
    lookup: impl Fn(&mut [libc::c_char], *mut libc::passwd, *mut *mut libc::passwd) -> libc::c_int,
    field: impl Fn(&libc::passwd) -> *mut libc::c_char,
) -> Option<String> {
    let mut buf = vec![0 as libc::c_char; 4096];
    loop {
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        let rc = lookup(&mut buf, &mut pw, &mut out);
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || out.is_null() {
            return None;
        }
        let p = field(&pw);
        if p.is_null() {
            return None;
        }
        return Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gecos_parsing() {
        assert_eq!(gecos_full_name("Sigma Studio,,,"), Some("Sigma Studio"));
        assert_eq!(gecos_full_name("  Ada Lovelace "), Some("Ada Lovelace"));
        assert_eq!(gecos_full_name(""), None);
        assert_eq!(gecos_full_name(",room,phone"), None);
    }

    #[test]
    fn display_name_falls_back_to_login() {
        assert_eq!(display_name_from("sigma", Some("Sigma Studio,,,")), "Sigma Studio");
        assert_eq!(display_name_from("sigma", Some("")), "sigma");
        assert_eq!(display_name_from("sigma", None), "sigma");
        assert_eq!(display_name_from("ada", Some("& Lovelace")), "Ada Lovelace");
    }

    #[test]
    fn nss_lookup() {
        // root exists everywhere, including the Nix build sandbox.
        assert!(!display_name("root").is_empty());
        assert_eq!(display_name("garde-no-such-user"), "garde-no-such-user");
        assert!(current_login().is_some());
    }
}

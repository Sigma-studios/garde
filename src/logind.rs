//! logind over D-Bus: power actions for the login screen, lock triggers for the lock daemon.

use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::zvariant::{OwnedFd, OwnedObjectPath};

const DEST: &str = "org.freedesktop.login1";
const PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const SESSION: &str = "org.freedesktop.login1.Session";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    PowerOff,
    Reboot,
}

/// Ask logind to power off or reboot (`interactive = true` lets polkit decide).
pub fn power(action: Power) -> Result<(), String> {
    let conn = Connection::system().map_err(|e| e.to_string())?;
    let method = match action {
        Power::PowerOff => "PowerOff",
        Power::Reboot => "Reboot",
    };
    conn.call_method(Some(DEST), PATH, Some(MANAGER), method, &(true,)).map_err(|e| e.to_string())?;
    Ok(())
}

/// What the lock daemon reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Lock,
    Unlock,
    /// PrepareForSleep(true): lock now, the system is about to sleep.
    Sleep,
    /// PrepareForSleep(false): resumed.
    Resume,
}

pub struct Logind {
    conn: Connection,
    pub session: OwnedObjectPath,
    inhibit_warned: std::sync::atomic::AtomicBool,
}

impl Logind {
    pub fn connect() -> zbus::Result<Self> {
        let conn = Connection::system()?;
        let session = find_session(&conn)?;
        log::info!("watching logind session {}", session.as_str());
        Ok(Self { conn, session, inhibit_warned: Default::default() })
    }

    /// Delay inhibitor so the screen is locked before the system sleeps. Dropping the fd releases it.
    pub fn inhibit_sleep(&self) -> Option<OwnedFd> {
        let r = self.conn.call_method(
            Some(DEST),
            PATH,
            Some(MANAGER),
            "Inhibit",
            &("sleep", "garde", "Lock the screen before sleep", "delay"),
        );
        match r.and_then(|m| m.body().deserialize::<OwnedFd>()) {
            Ok(fd) => Some(fd),
            Err(e) => {
                if !self.inhibit_warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    log::warn!("no sleep inhibitor (the screen may lock only after resume): {e}");
                }
                None
            }
        }
    }

    pub fn set_locked_hint(&self, locked: bool) {
        if let Err(e) =
            self.conn.call_method(Some(DEST), self.session.as_str(), Some(SESSION), "SetLockedHint", &(locked,))
        {
            log::warn!("SetLockedHint({locked}): {e}");
        }
    }

    /// Block forever, calling `f` for each signal of interest. Returns on D-Bus errors.
    pub fn listen(&self, mut f: impl FnMut(Signal)) -> zbus::Result<()> {
        let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).sender(DEST)?.build();
        let it = MessageIterator::for_match_rule(rule, &self.conn, Some(64))?;
        // The match rule's well-known sender is not enforced locally, and anyone on the bus can
        // send a signal straight to our unique name: check the real sender ourselves, or a
        // forged `Unlock` would open the screen.
        let dbus = zbus::blocking::fdo::DBusProxy::new(&self.conn)?;
        let owner = || dbus.get_name_owner(DEST.try_into().expect("valid name")).ok().map(|o| o.to_string());
        let mut logind = owner();
        for msg in it {
            let msg = msg?;
            let h = msg.header();
            let (Some(member), Some(path), Some(iface)) = (h.member(), h.path(), h.interface()) else { continue };
            let sender = h.sender().map(|s| s.to_string());
            if h.destination().is_some() {
                log::warn!("ignoring a unicast {member} signal from {sender:?}");
                continue;
            }
            if sender.is_none() || sender != logind {
                // logind may have restarted under a new name.
                logind = owner();
                if sender.is_none() || sender != logind {
                    log::warn!("ignoring {member} from {sender:?}: not logind ({logind:?})");
                    continue;
                }
            }
            let on_session = iface.as_str() == SESSION && path.as_str() == self.session.as_str();
            let sig = match member.as_str() {
                "Lock" if on_session => Signal::Lock,
                "Unlock" if on_session => Signal::Unlock,
                "PrepareForSleep" if iface.as_str() == MANAGER && path.as_str() == PATH => {
                    match msg.body().deserialize::<bool>() {
                        Ok(true) => Signal::Sleep,
                        Ok(false) => Signal::Resume,
                        Err(_) => continue,
                    }
                }
                _ => continue,
            };
            f(sig);
        }
        Ok(())
    }
}

/// Our logind session: $XDG_SESSION_ID, else the session of this process, else the user's
/// display session (a systemd user service belongs to no session).
fn find_session(conn: &Connection) -> zbus::Result<OwnedObjectPath> {
    let mgr = Proxy::new(conn, DEST, PATH, MANAGER)?;
    if let Ok(id) = std::env::var("XDG_SESSION_ID") {
        if let Ok(p) = mgr.call::<_, _, OwnedObjectPath>("GetSession", &(id.as_str(),)) {
            return Ok(p);
        }
    }
    if let Ok(p) = mgr.call::<_, _, OwnedObjectPath>("GetSessionByPID", &(std::process::id(),)) {
        return Ok(p);
    }
    let uid = unsafe { libc::getuid() };
    let user: OwnedObjectPath = mgr.call("GetUser", &(uid,))?;
    let user = Proxy::new(conn, DEST, user.as_str(), "org.freedesktop.login1.User")?;
    let (id, path): (String, OwnedObjectPath) = user.get_property("Display")?;
    if id.is_empty() {
        return Err(zbus::Error::Failure("user has no graphical session".into()));
    }
    Ok(path)
}

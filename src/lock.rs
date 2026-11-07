//! `garde lock`: ext-session-lock-v1 locker.
//!
//! Safety rule: the only call to `unlock` is after PAM said yes or logind sent Unlock. Every error
//! path exits or keeps the lock; the protocol keeps the session locked if we die.

use std::path::PathBuf;
use std::sync::Arc;

use smithay_client_toolkit::{
    reexports::calloop::channel::{self, Event as ChannelEvent, Sender},
    session_lock::SessionLock,
};
use wayland_client::protocol::wl_output;
use zbus::zvariant::OwnedFd;

use crate::app::{App, ModeState, OutSurface, SurfKind};
use crate::auth;
use crate::keyboard::Action;
use crate::logind::{Logind, Signal};
use crate::ui::Tone;

pub enum AuthEvent {
    Message(auth::Message),
    Done(Result<(), String>),
}

pub struct Lock {
    daemon: bool,
    lock: Option<SessionLock>,
    locked: bool,
    unlock_requested: bool,
    checking: bool,
    /// Bumped on every lock and unlock: a PAM result only counts for the lock it was started in.
    generation: u64,
    logind: Option<Arc<Logind>>,
    inhibitor: Option<OwnedFd>,
    marker: Option<PathBuf>,
    auth_tx: Option<Sender<(u64, AuthEvent)>>,
}

impl Lock {
    pub fn new(daemon: bool) -> Self {
        Self {
            daemon,
            lock: None,
            locked: false,
            unlock_requested: false,
            checking: false,
            generation: 0,
            logind: None,
            inhibitor: None,
            marker: None,
            auth_tx: None,
        }
    }
}

fn this(app: &mut App) -> &mut Lock {
    match &mut app.mode {
        ModeState::Lock(l) => l,
        ModeState::Login(_) => unreachable!(),
    }
}

pub fn init(app: &mut App) -> Result<(), String> {
    let (tx, rx) = channel::channel::<(u64, AuthEvent)>();
    app.handle
        .insert_source(rx, |ev, _, app: &mut App| {
            if let ChannelEvent::Msg((generation, e)) = ev {
                on_auth(app, generation, e);
            }
        })
        .map_err(|e| e.to_string())?;
    this(app).auth_tx = Some(tx);

    let user = crate::user::current_login();
    if user.as_deref() != Some(app.cfg.user.as_str()) {
        log::warn!("configured user {:?} is not the user running garde ({user:?}); PAM may refuse", app.cfg.user);
    }

    let daemon = this(app).daemon;
    match Logind::connect() {
        Ok(l) => {
            let l = Arc::new(l);
            let id = l.session.as_str().rsplit('/').next().unwrap_or("session").to_string();
            this(app).marker =
                std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join(format!("garde-{id}.locked")));
            this(app).logind = Some(l);
        }
        Err(e) if daemon => return Err(format!("logind: {e}")),
        Err(e) => log::warn!("logind unavailable, no locked hint: {e}"),
    }

    if daemon {
        let l = this(app).logind.clone().expect("checked above");
        this(app).inhibitor = l.inhibit_sleep();
        let (stx, srx) = channel::channel::<Signal>();
        app.handle
            .insert_source(srx, |ev, _, app: &mut App| match ev {
                ChannelEvent::Msg(s) => on_signal(app, s),
                ChannelEvent::Closed => {
                    // Lost logind: exit (never unlock); systemd restarts us and the marker
                    // file makes the new instance take the lock back over.
                    log::error!("logind signal stream ended");
                    app.exit(1);
                }
            })
            .map_err(|e| e.to_string())?;
        std::thread::Builder::new()
            .name("logind".into())
            .spawn(move || {
                if let Err(e) = l.listen(|s| {
                    let _ = stx.send(s);
                }) {
                    log::error!("logind: {e}");
                }
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// After the initial roundtrip (outputs known).
pub fn start_initial(app: &mut App) -> Result<(), String> {
    let l = this(app);
    if !l.daemon {
        return start(app);
    }
    // A previous instance died while locked: take the (abandoned) lock back so there's a
    // password prompt again instead of a blank screen.
    if l.marker.as_ref().is_some_and(|m| m.exists()) {
        log::warn!("lock marker found: relocking after a restart");
        return start(app);
    }
    Ok(())
}

fn on_signal(app: &mut App, s: Signal) {
    log::info!("logind: {s:?}");
    match s {
        Signal::Lock => {
            if let Err(e) = start(app) {
                log::error!("{e}");
            }
        }
        Signal::Sleep => {
            let l = this(app);
            if l.locked {
                l.inhibitor = None;
            } else if let Err(e) = start(app) {
                log::error!("{e}");
                this(app).inhibitor = None;
            }
        }
        Signal::Resume => {
            let l = this(app);
            if l.inhibitor.is_none() {
                l.inhibitor = l.logind.as_ref().and_then(|lg| lg.inhibit_sleep());
            }
        }
        // logind (an authorized `loginctl unlock-session`) asked: honored, as every desktop does.
        Signal::Unlock => unlock(app),
    }
}

/// Lock now: request the lock and put a lock surface on every output.
pub fn start(app: &mut App) -> Result<(), String> {
    if this(app).lock.is_some() {
        return Ok(());
    }
    let state = app.lock_state.as_ref().ok_or("compositor has no ext-session-lock-v1")?;
    let lock = state.lock(&app.qh).map_err(|_| "compositor has no ext-session-lock-v1".to_string())?;
    let l = this(app);
    l.lock = Some(lock);
    l.locked = false;
    l.unlock_requested = false;
    l.checking = false;
    l.generation += 1;
    app.ui.clear();
    app.ui.busy = false;
    app.ui.masked = true;
    app.ui.placeholder = "Password".into();
    app.ui.status = None;
    app.ui.reveal = crate::ui::Reveal::new();
    let outputs: Vec<_> = app.outputs.outputs().collect();
    for o in outputs {
        add_surface(app, o);
    }
    app.start_blink();
    log::info!("locking");
    Ok(())
}

fn add_surface(app: &mut App, output: wl_output::WlOutput) {
    let Some(lock) = this(app).lock.clone() else { return };
    if app.surfaces.iter().any(|s| s.output.as_ref() == Some(&output)) {
        return;
    }
    let surface = app.compositor.create_surface(&app.qh);
    let ls = lock.create_lock_surface(surface, &output, &app.qh);
    app.surfaces.push(OutSurface::new(Some(output), SurfKind::Lock(ls)));
}

pub fn new_output(app: &mut App, output: wl_output::WlOutput) {
    if this(app).lock.is_some() {
        add_surface(app, output);
    }
}

pub fn locked(app: &mut App, _lock: SessionLock) {
    log::info!("locked");
    let l = this(app);
    l.locked = true;
    // The screen is covered: sleep may proceed.
    l.inhibitor = None;
    if let Some(m) = &l.marker {
        if let Err(e) = std::fs::write(m, b"") {
            log::warn!("{}: {e}", m.display());
        }
    }
    if let Some(lg) = &l.logind {
        lg.set_locked_hint(true);
    }
    if l.unlock_requested {
        unlock(app);
    }
}

pub fn finished(app: &mut App, _lock: SessionLock) {
    let l = this(app);
    let was_locked = l.locked;
    if !l.daemon {
        log::error!("the compositor refused the lock (another locker is active?)");
        // Not unlocking: just leave. If we were locked, the compositor ended it itself.
        app.exit(1);
        return;
    }
    if was_locked {
        log::warn!("the compositor ended the lock");
    } else {
        log::error!("the compositor refused the lock (another locker is active?)");
    }
    // After `locked`, the protocol only allows unlock_and_destroy, even once the compositor has
    // ended the lock itself (it's already over, so this opens nothing).
    if let (true, Some(lock)) = (was_locked, l.lock.take()) {
        lock.unlock();
    }
    l.lock = None;
    l.locked = false;
    l.checking = false;
    l.generation += 1;
    if let Some(m) = &l.marker {
        let _ = std::fs::remove_file(m);
    }
    if was_locked {
        if let Some(lg) = &l.logind {
            lg.set_locked_hint(false);
        }
    }
    app.surfaces.clear();
    app.stop_blink();
    app.ui.clear();
}

/// The one way out. Only reached after a successful PAM check or logind's Unlock.
fn unlock(app: &mut App) {
    let l = this(app);
    let Some(lock) = l.lock.clone() else { return };
    if !l.locked {
        // Not locked yet: unlocking now is a protocol error; do it once `locked` arrives.
        l.unlock_requested = true;
        return;
    }
    lock.unlock();
    l.lock = None;
    l.locked = false;
    l.checking = false;
    l.generation += 1;
    if let Some(m) = &l.marker {
        let _ = std::fs::remove_file(m);
    }
    if let Some(lg) = &l.logind {
        lg.set_locked_hint(false);
    }
    let daemon = l.daemon;
    if daemon && l.inhibitor.is_none() {
        l.inhibitor = l.logind.as_ref().and_then(|lg| lg.inhibit_sleep());
    }
    app.surfaces.clear();
    app.stop_blink();
    app.ui.clear();
    app.ui.status = None;
    let _ = app.conn.roundtrip();
    log::info!("unlocked");
    if !daemon {
        app.exit(0);
    }
}

pub fn submit(app: &mut App) {
    // Enter on an empty field (often just to wake the screen) must not cost a faillock attempt.
    if app.ui.password().is_empty() {
        return;
    }
    let service = app.cfg.pam_service.clone();
    let user = app.cfg.user.clone();
    let l = this(app);
    if !l.locked || l.checking {
        return;
    }
    let Some(tx) = l.auth_tx.clone() else { return };
    l.checking = true;
    let generation = l.generation;
    let password = app.ui.take_password();
    app.ui.busy = true;
    app.ui.set_status(Tone::Busy, "Checking…");
    let spawned = std::thread::Builder::new().name("pam".into()).spawn(move || {
        let msg_tx = tx.clone();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            auth::check(&service, &user, password, |m| {
                let _ = msg_tx.send((generation, AuthEvent::Message(m)));
            })
        }))
        .unwrap_or_else(|_| Err("Authentication failed (internal error)".into()));
        let _ = tx.send((generation, AuthEvent::Done(r)));
    });
    if let Err(e) = spawned {
        this(app).checking = false;
        app.ui.busy = false;
        app.ui.set_status(Tone::Error, format!("Cannot check the password: {e}"));
    }
}

fn on_auth(app: &mut App, generation: u64, ev: AuthEvent) {
    let l = this(app);
    if generation != l.generation || !l.checking {
        // From a lock that's already over (unlocked by logind, or ended by the compositor).
        log::debug!("ignoring a stale PAM result");
        return;
    }
    match ev {
        AuthEvent::Message(m) => app.ui.set_status(if m.error { Tone::Error } else { Tone::Info }, m.text),
        AuthEvent::Done(Ok(())) => {
            log::info!("password accepted");
            unlock(app);
            return;
        }
        AuthEvent::Done(Err(msg)) => {
            log::info!("password rejected: {msg}");
            this(app).checking = false;
            app.ui.busy = false;
            app.ui.clear();
            app.ui.set_status(Tone::Error, msg);
        }
    }
    app.redraw_card();
}

pub fn key(app: &mut App, action: &Action) {
    if let Action::Escape = action {
        if !app.ui.busy {
            app.ui.clear();
        }
    }
}

pub fn pick(_app: &mut App, _d: i32) {}

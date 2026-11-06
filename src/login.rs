//! `garde login`: the greetd greeter.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use smithay_client_toolkit::reexports::calloop::{
    channel::{self, Event as ChannelEvent},
    timer::{TimeoutAction, Timer},
};
use wayland_client::protocol::wl_output;

use crate::app::{App, ModeState};
use crate::config::State;
use crate::greetd::{self, Cmd, Event};
use crate::keyboard::Action;
use crate::logind::{self, Power};
use crate::sessions::{self, Session};
use crate::ui::Tone;

const CONFIRM_FOR: Duration = Duration::from_secs(6);
pub const HINTS: &str = "F1  Shut down        F2  Reboot";

pub struct Login {
    sessions: Vec<Session>,
    selected: usize,
    tx: Option<mpsc::Sender<Cmd>>,
    power: Option<(Power, Instant)>,
}

impl Login {
    pub fn new(cfg: &crate::config::Config) -> Self {
        let sessions = sessions::discover(&cfg.sessions.dirs);
        let state = State::load(&cfg.state_file);
        let selected = sessions::default_index(&sessions, state.last_session.as_deref());
        log::info!("sessions: {:?}", sessions.iter().map(|s| &s.id).collect::<Vec<_>>());
        Self { sessions, selected, tx: None, power: None }
    }
}

fn this(app: &mut App) -> &mut Login {
    match &mut app.mode {
        ModeState::Login(l) => l,
        ModeState::Lock(_) => unreachable!(),
    }
}

/// Start the greetd worker and fill in the login-only parts of the card.
pub fn init(app: &mut App) -> Result<(), String> {
    app.ui.hints = Some(HINTS.into());
    refresh_session(app);
    if this(app).sessions.is_empty() {
        app.ui.set_status(Tone::Error, "No session found");
    }

    let (etx, erx) = channel::channel::<Event>();
    app.handle
        .insert_source(erx, |ev, _, app: &mut App| {
            if let ChannelEvent::Msg(e) = ev {
                on_greetd(app, e);
            }
        })
        .map_err(|e| e.to_string())?;
    let (ctx, crx) = mpsc::channel();
    this(app).tx = Some(ctx);
    let user = app.cfg.user.clone();
    std::thread::Builder::new()
        .name("greetd".into())
        .spawn(move || match greetd::connect() {
            Ok(s) => greetd::run(s, &user, crx, |e| {
                let _ = etx.send(e);
            }),
            Err(e) => {
                let _ = etx.send(Event::Fatal(format!("Cannot reach greetd: {e}")));
            }
        })
        .map_err(|e| e.to_string())?;
    app.start_blink();
    Ok(())
}

fn refresh_session(app: &mut App) {
    let l = this(app);
    let row = match l.sessions.get(l.selected) {
        Some(s) => (s.name.clone(), l.sessions.len() > 1),
        None => ("No session".into(), false),
    };
    app.ui.session = Some(row);
}

fn on_greetd(app: &mut App, ev: Event) {
    log::debug!("greetd: {ev:?}");
    match ev {
        Event::Prompt { secret, text } => {
            app.ui.set_prompt(secret, &text);
            if app.ui.busy {
                app.ui.busy = false;
                if app.ui.status.as_ref().is_some_and(|s| s.tone == Tone::Busy) {
                    app.ui.status = None;
                }
            }
        }
        Event::Message { error, text } => app.ui.set_status(if error { Tone::Error } else { Tone::Info }, text),
        Event::Failed(msg) => {
            app.ui.busy = false;
            app.ui.clear();
            app.ui.masked = true;
            app.ui.set_status(Tone::Error, msg);
        }
        Event::Started => {
            let l = this(app);
            if let Some(s) = l.sessions.get(l.selected) {
                let st = State { last_session: Some(s.id.clone()) };
                if let Err(e) = st.save(&app.cfg.state_file) {
                    log::warn!("saving {}: {e}", app.cfg.state_file.display());
                }
            }
            log::info!("session started");
            app.exit(0);
        }
        Event::Fatal(msg) => {
            log::error!("{msg}");
            app.ui.busy = false;
            app.ui.set_status(Tone::Error, msg);
            // Let greetd restart us rather than sitting on a dead connection.
            let _ = app.handle.insert_source(Timer::from_duration(Duration::from_secs(5)), |_, _, app: &mut App| {
                app.exit(1);
                TimeoutAction::Drop
            });
        }
    }
    app.redraw_card();
}

pub fn submit(app: &mut App) {
    // An empty hidden prompt isn't an attempt worth sending (it would count against faillock).
    if app.ui.masked && app.ui.password().is_empty() {
        return;
    }
    let wrapper = app.cfg.xsession_wrapper.clone();
    let l = this(app);
    let Some(session) = l.sessions.get(l.selected) else {
        app.ui.set_status(Tone::Error, "No session found");
        return;
    };
    let (cmd, env) = session.launch(&wrapper);
    let Some(tx) = l.tx.clone() else { return };
    let password = app.ui.take_password();
    if tx.send(Cmd::Submit { password, cmd, env }).is_err() {
        app.ui.set_status(Tone::Error, "Lost the connection to greetd");
        return;
    }
    app.ui.busy = true;
    app.ui.set_status(Tone::Busy, "Checking…");
}

pub fn key(app: &mut App, action: &Action) {
    let power = match action {
        Action::F(1) => Some(Power::PowerOff),
        Action::F(2) => Some(Power::Reboot),
        _ => None,
    };
    let pending = this(app).power.take();
    match (power, pending) {
        (Some(p), Some((q, at))) if p == q && at.elapsed() < CONFIRM_FOR => {
            app.ui.set_status(Tone::Busy, if p == Power::PowerOff { "Shutting down…" } else { "Rebooting…" });
            run_power(app, p);
        }
        (Some(p), _) => {
            this(app).power = Some((p, Instant::now()));
            let (k, what) = if p == Power::PowerOff { ("F1", "shut down") } else { ("F2", "reboot") };
            app.ui.set_status(Tone::Info, format!("Press {k} again to {what}"));
        }
        (None, Some(_)) => {
            // Any other key cancels the confirmation.
            if app.ui.status.as_ref().is_some_and(|s| s.text.starts_with("Press F")) {
                app.ui.status = None;
            }
        }
        (None, None) => {}
    }
    match action {
        Action::Left => pick(app, -1),
        Action::Right => pick(app, 1),
        Action::Escape if !app.ui.busy => app.ui.clear(),
        _ => {}
    }
}

fn run_power(app: &mut App, p: Power) {
    let (tx, rx) = channel::channel::<String>();
    let _ = app.handle.insert_source(rx, |ev, _, app: &mut App| {
        if let ChannelEvent::Msg(err) = ev {
            app.ui.set_status(Tone::Error, err);
            app.redraw_card();
        }
    });
    std::thread::spawn(move || {
        if let Err(e) = logind::power(p) {
            let _ = tx.send(format!("Could not {}: {e}", if p == Power::PowerOff { "shut down" } else { "reboot" }));
        }
    });
}

pub fn pick(app: &mut App, d: i32) {
    if app.ui.busy {
        return;
    }
    let l = this(app);
    let n = l.sessions.len() as i32;
    if n > 1 {
        l.selected = (l.selected as i32 + d).rem_euclid(n) as usize;
        refresh_session(app);
    }
}

pub fn new_output(app: &mut App, output: wl_output::WlOutput) {
    // Layer-shell: one surface per output. Without it (cage) a single fullscreen window.
    if app.layer_shell.is_some() || app.surfaces.is_empty() {
        app.add_login_surface(Some(output));
    }
}

/// After the initial roundtrip: make sure something is on screen even with no wl_output yet.
pub fn ensure_surface(app: &mut App) {
    if app.surfaces.is_empty() {
        app.add_login_surface(None);
    }
}

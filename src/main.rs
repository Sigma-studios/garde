//! garde: a greetd login screen and a Wayland lock screen that look the same.

mod app;
mod auth;
mod config;
mod greetd;
mod keyboard;
mod lock;
mod login;
mod logind;
mod sessions;
mod ui;
mod user;

use std::path::PathBuf;
use std::process::ExitCode;

use smithay_client_toolkit::reexports::{calloop::EventLoop, calloop_wayland_source::WaylandSource};
use wayland_client::{globals::registry_queue_init, Connection};

use app::{App, ModeState};

const USAGE: &str = "\
usage: garde login [--config PATH]
       garde lock [--daemon] [--config PATH]

  login     greetd greeter (run it under cage: cage -s -- garde login)
  lock      lock the session now (ext-session-lock-v1), exit once unlocked
  --daemon  stay running, lock on logind Lock / PrepareForSleep, unlock on Unlock
  --config  config file (default /etc/garde/config.toml, or $GARDE_CONFIG)";

/// Exit code when the compositor can't lock at all (e.g. KWin): distinct, so a service can
/// tell "nothing to do here" from a failure.
const EXIT_UNSUPPORTED: u8 = 3;

enum Cmd {
    Login,
    Lock { daemon: bool },
}

fn parse_args() -> Result<(Cmd, PathBuf), String> {
    let mut args = std::env::args().skip(1);
    let mut cmd = None;
    let mut daemon = false;
    let mut config =
        std::env::var_os("GARDE_CONFIG").map(PathBuf::from).unwrap_or_else(|| config::DEFAULT_CONFIG.into());
    while let Some(a) = args.next() {
        match a.as_str() {
            "login" if cmd.is_none() => cmd = Some("login"),
            "lock" if cmd.is_none() => cmd = Some("lock"),
            "--daemon" => daemon = true,
            "--config" => config = args.next().ok_or("--config needs a path")?.into(),
            "-h" | "--help" => return Err(String::new()),
            "-V" | "--version" => {
                println!("garde {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    match cmd {
        Some("login") if !daemon => Ok((Cmd::Login, config)),
        Some("lock") => Ok((Cmd::Lock { daemon }, config)),
        Some(_) => Err("--daemon only applies to lock".into()),
        None => Err(String::new()),
    }
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,zbus=warn,tracing=warn")).init();
    let (cmd, config_path) = match parse_args() {
        Ok(v) => v,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("garde: {e}");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(cmd, &config_path) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            log::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cmd: Cmd, config_path: &std::path::Path) -> Result<u8, String> {
    let cfg = config::Config::load(config_path)?;
    let conn = Connection::connect_to_env().map_err(|e| format!("Wayland: {e}"))?;
    let (globals, mut queue) = registry_queue_init::<App>(&conn).map_err(|e| format!("Wayland registry: {e}"))?;
    let qh = queue.handle();

    let login = matches!(cmd, Cmd::Login);
    if !login {
        let has_lock = globals.contents().with_list(|l| l.iter().any(|g| g.interface == "ext_session_lock_manager_v1"));
        if !has_lock {
            log::error!("this compositor does not support ext-session-lock-v1 (KWin doesn't); nothing to do");
            return Ok(EXIT_UNSUPPORTED);
        }
    }

    let mut event_loop: EventLoop<'static, App> = EventLoop::try_new().map_err(|e| e.to_string())?;
    let mode = match cmd {
        Cmd::Login => ModeState::Login(login::Login::new(&cfg)),
        Cmd::Lock { daemon } => ModeState::Lock(lock::Lock::new(daemon)),
    };
    let mut app = App::new(conn.clone(), &globals, qh, event_loop.handle(), event_loop.get_signal(), cfg, login, mode)?;
    match app.mode {
        ModeState::Login(_) => login::init(&mut app)?,
        ModeState::Lock(_) => lock::init(&mut app)?,
    }
    // Learn outputs and seats (new_output creates the login surfaces).
    queue.roundtrip(&mut app).map_err(|e| e.to_string())?;
    match app.mode {
        ModeState::Login(_) => login::ensure_surface(&mut app),
        ModeState::Lock(_) => lock::start_initial(&mut app)?,
    }
    WaylandSource::new(conn, queue).insert(event_loop.handle()).map_err(|e| e.to_string())?;

    event_loop.run(None, &mut app, |app| app.flush()).map_err(|e| e.to_string())?;
    Ok(app.exit_code.unwrap_or(0) as u8)
}

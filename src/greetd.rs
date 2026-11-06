//! greetd client. Runs on its own thread with a blocking socket, talks to the UI through
//! channels. Generic over the stream so tests can drive it against a mock greetd.

use std::io::{Read, Write};
use std::sync::mpsc::Receiver;

use greetd_ipc::{AuthMessageType, ErrorType, Request, Response};
use zeroize::{Zeroize, Zeroizing};

/// What the UI asks the worker to do.
pub enum Cmd {
    /// Enter pressed: the password (answer to the pending prompt) and what to launch on success.
    Submit { password: Zeroizing<String>, cmd: Vec<String>, env: Vec<String> },
}

/// What the worker tells the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// greetd wants an answer; `secret` means mask it.
    Prompt { secret: bool, text: String },
    /// An info or error message from PAM, to show verbatim.
    Message { error: bool, text: String },
    /// The attempt failed; a new session is being created for the next one.
    Failed(String),
    /// StartSession succeeded: the greeter should exit now.
    Started,
    /// The connection to greetd is gone.
    Fatal(String),
}

pub const WRONG_PASSWORD: &str = "Wrong password";

/// PAM messages of one attempt. A failure is explained by its error messages if any, else its
/// info messages (faillock's "account locked" is info, and comes before the password prompt
/// because `auth required pam_faillock.so preauth` lets the stack continue), else nothing.
#[derive(Debug, Default)]
pub struct Messages {
    errors: Vec<String>,
    infos: Vec<String>,
}

impl Messages {
    pub fn push(&mut self, error: bool, text: &str) {
        let text = text.trim();
        if !text.is_empty() {
            if error { &mut self.errors } else { &mut self.infos }.push(text.to_string());
        }
    }

    pub fn explanation(&self) -> Option<String> {
        let m = if self.errors.is_empty() { &self.infos } else { &self.errors };
        (!m.is_empty()).then(|| m.join(" "))
    }
}

fn write_req(s: &mut impl Write, req: &Request) -> std::io::Result<()> {
    // Serialize into a buffer we wipe afterwards: it may hold the password.
    let mut body = Zeroizing::new(Vec::with_capacity(4096));
    serde_json::to_writer(&mut *body, req).map_err(std::io::Error::other)?;
    let mut msg = Zeroizing::new(Vec::with_capacity(body.len() + 4));
    msg.extend_from_slice(&(body.len() as u32).to_ne_bytes());
    msg.extend_from_slice(&body);
    s.write_all(&msg)?;
    s.flush()
}

fn read_resp(s: &mut impl Read) -> std::io::Result<Response> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len)?;
    let len = u32::from_ne_bytes(len) as usize;
    if len > 1 << 20 {
        return Err(std::io::Error::other("oversized greetd message"));
    }
    let mut body = vec![0u8; len];
    s.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(std::io::Error::other)
}

fn round_trip<S: Read + Write>(s: &mut S, req: &Request) -> std::io::Result<Response> {
    write_req(s, req)?;
    read_resp(s)
}

struct Pending {
    password: Zeroizing<String>,
    cmd: Vec<String>,
    env: Vec<String>,
}

fn recv(cmds: &Receiver<Cmd>) -> Option<Pending> {
    match cmds.recv().ok()? {
        Cmd::Submit { password, cmd, env } => Some(Pending { password, cmd, env }),
    }
}

/// Drive greetd until a session starts or the UI goes away. Never returns on a wrong password.
pub fn run<S: Read + Write>(mut stream: S, user: &str, cmds: Receiver<Cmd>, emit: impl Fn(Event)) {
    if let Err(e) = drive(&mut stream, user, &cmds, &emit) {
        emit(Event::Fatal(format!("greetd: {e}")));
    }
}

fn drive<S: Read + Write>(s: &mut S, user: &str, cmds: &Receiver<Cmd>, emit: &impl Fn(Event)) -> std::io::Result<()> {
    // A submit typed while no prompt was pending (e.g. right after a failure that never asked
    // for a password); it answers the next prompt.
    let mut queued: Option<Pending> = None;
    loop {
        let mut resp = round_trip(s, &Request::CreateSession { username: user.to_string() })?;
        // The Enter press this attempt answers with; spent whether or not a prompt comes.
        let mut attempt = queued.take();
        let mut answered = false;
        let mut msgs = Messages::default();
        let mut launch: Option<(Vec<String>, Vec<String>)> = None;
        let mut starting = false;
        loop {
            match resp {
                Response::AuthMessage {
                    auth_message_type: t @ (AuthMessageType::Secret | AuthMessageType::Visible),
                    auth_message,
                } => {
                    let p = match attempt.take() {
                        Some(p) => p,
                        None => {
                            emit(Event::Prompt { secret: matches!(t, AuthMessageType::Secret), text: auth_message });
                            let Some(p) = recv(cmds) else { return Ok(()) };
                            p
                        }
                    };
                    answered = true;
                    let Pending { password, cmd, env } = p;
                    launch = Some((cmd, env));
                    let mut req = Request::PostAuthMessageResponse { response: Some(password.to_string()) };
                    drop(password);
                    let r = round_trip(s, &req);
                    if let Request::PostAuthMessageResponse { response: Some(pw) } = &mut req {
                        pw.zeroize();
                    }
                    resp = r?;
                }
                Response::AuthMessage { auth_message_type: t, auth_message } => {
                    let error = matches!(t, AuthMessageType::Error);
                    msgs.push(error, &auth_message);
                    emit(Event::Message { error, text: auth_message });
                    resp = round_trip(s, &Request::PostAuthMessageResponse { response: None })?;
                }
                Response::Success if starting => {
                    emit(Event::Started);
                    return Ok(());
                }
                Response::Success => {
                    // Authenticated. Without any prompt (passwordless account) wait for Enter so
                    // the session picker still means something.
                    let (cmd, env) = match launch.take().or_else(|| attempt.take().map(|p| (p.cmd, p.env))) {
                        Some(l) => l,
                        None => {
                            emit(Event::Prompt { secret: false, text: String::new() });
                            let Some(p) = recv(cmds) else { return Ok(()) };
                            (p.cmd, p.env)
                        }
                    };
                    starting = true;
                    resp = round_trip(s, &Request::StartSession { cmd, env })?;
                }
                Response::Error { error_type, description } => {
                    let msg = match error_type {
                        // Prefer what PAM said (faillock's "account locked", sent as info).
                        ErrorType::AuthError => msgs.explanation().unwrap_or_else(|| WRONG_PASSWORD.into()),
                        ErrorType::Error => description,
                    };
                    emit(Event::Failed(msg));
                    // Best effort: greetd may already have dropped the session.
                    let _ = round_trip(s, &Request::CancelSession)?;
                    if !answered {
                        // Failed without asking anything (e.g. a locked account): don't spin, wait
                        // for the user to try again.
                        let Some(p) = recv(cmds) else { return Ok(()) };
                        queued = Some(p);
                    }
                    break;
                }
            }
        }
    }
}

/// Connect to `$GREETD_SOCK`.
pub fn connect() -> std::io::Result<std::os::unix::net::UnixStream> {
    let path = std::env::var_os("GREETD_SOCK").ok_or_else(|| std::io::Error::other("GREETD_SOCK is not set"))?;
    std::os::unix::net::UnixStream::connect(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::thread;

    fn send(s: &mut UnixStream, r: Response) {
        let body = serde_json::to_vec(&r).unwrap();
        s.write_all(&(body.len() as u32).to_ne_bytes()).unwrap();
        s.write_all(&body).unwrap();
    }

    fn recv_req(s: &mut UnixStream) -> serde_json::Value {
        let mut len = [0u8; 4];
        s.read_exact(&mut len).unwrap();
        let mut b = vec![0u8; u32::from_ne_bytes(len) as usize];
        s.read_exact(&mut b).unwrap();
        serde_json::from_slice(&b).unwrap()
    }

    fn secret(text: &str) -> Response {
        Response::AuthMessage { auth_message_type: AuthMessageType::Secret, auth_message: text.into() }
    }

    fn submit(tx: &mpsc::Sender<Cmd>, pw: &str) {
        tx.send(Cmd::Submit {
            password: Zeroizing::new(pw.into()),
            cmd: vec!["start-cosmic".into()],
            env: vec!["XDG_SESSION_TYPE=wayland".into()],
        })
        .unwrap();
    }

    #[derive(Clone, Copy)]
    enum Lockout {
        /// `auth required pam_faillock.so preauth`: says the account is locked (info), then
        /// still asks for the password and fails.
        Required,
        /// `auth requisite ...`: fails right away without asking anything.
        Requisite,
    }

    const LOCKED: &str = "The account is locked due to 2 failed logins.";
    const LEFT: &str = "(10 minutes left to unlock)";

    /// A mock greetd accepting password "right", locking the account after two failures.
    fn mock_greetd(mut s: UnixStream, style: Lockout) {
        let mut failures = 0;
        loop {
            let req = recv_req(&mut s);
            match req["type"].as_str().unwrap() {
                "create_session" => {
                    assert_eq!(req["username"], "sigma");
                    let locked = failures >= 2;
                    if locked {
                        for m in [LOCKED, LEFT] {
                            let ty = if matches!(style, Lockout::Required) {
                                AuthMessageType::Info
                            } else {
                                AuthMessageType::Error
                            };
                            send(&mut s, Response::AuthMessage { auth_message_type: ty, auth_message: m.into() });
                            assert_eq!(recv_req(&mut s)["response"], serde_json::Value::Null);
                        }
                        if matches!(style, Lockout::Requisite) {
                            send(
                                &mut s,
                                Response::Error {
                                    error_type: ErrorType::AuthError,
                                    description: "pam_authenticate: AUTH_ERR".into(),
                                },
                            );
                            continue;
                        }
                    }
                    send(&mut s, secret("Password:"));
                    let r = recv_req(&mut s);
                    assert_eq!(r["type"], "post_auth_message_response");
                    if r["response"] == "right" && !locked {
                        send(&mut s, Response::Success);
                    } else {
                        failures += 1;
                        send(
                            &mut s,
                            Response::Error {
                                error_type: ErrorType::AuthError,
                                description: "pam_authenticate: AUTH_ERR".into(),
                            },
                        );
                    }
                }
                "start_session" => {
                    assert_eq!(req["cmd"], serde_json::json!(["start-cosmic"]));
                    assert_eq!(req["env"], serde_json::json!(["XDG_SESSION_TYPE=wayland"]));
                    send(&mut s, Response::Success);
                    return;
                }
                "cancel_session" => send(&mut s, Response::Success),
                other => panic!("unexpected {other}"),
            }
        }
    }

    fn start(
        style: Lockout,
    ) -> (mpsc::Sender<Cmd>, mpsc::Receiver<Event>, thread::JoinHandle<()>, thread::JoinHandle<()>) {
        let (a, b) = UnixStream::pair().unwrap();
        let mock = thread::spawn(move || mock_greetd(b, style));
        let (ctx, crx) = mpsc::channel();
        let (etx, erx) = mpsc::channel();
        let worker = thread::spawn(move || run(a, "sigma", crx, move |e| etx.send(e).unwrap()));
        (ctx, erx, mock, worker)
    }

    fn msg(error: bool, t: &str) -> Event {
        Event::Message { error, text: t.into() }
    }

    fn prompt() -> Event {
        Event::Prompt { secret: true, text: "Password:".into() }
    }

    fn fail_twice(tx: &mpsc::Sender<Cmd>, rx: &mpsc::Receiver<Event>) {
        for _ in 0..2 {
            assert_eq!(rx.recv().unwrap(), prompt());
            submit(tx, "wrong");
            assert_eq!(rx.recv().unwrap(), Event::Failed(WRONG_PASSWORD.into()));
        }
    }

    #[test]
    fn successful_login() {
        let (tx, rx, mock, worker) = start(Lockout::Required);
        assert_eq!(rx.recv().unwrap(), prompt());
        submit(&tx, "right");
        assert_eq!(rx.recv().unwrap(), Event::Started);
        worker.join().unwrap();
        mock.join().unwrap();
    }

    #[test]
    fn wrong_password_then_retry() {
        let (tx, rx, mock, worker) = start(Lockout::Required);
        assert_eq!(rx.recv().unwrap(), prompt());
        submit(&tx, "wrong");
        assert_eq!(rx.recv().unwrap(), Event::Failed(WRONG_PASSWORD.into()));
        // A fresh session for the same user, ready for the next try.
        assert_eq!(rx.recv().unwrap(), prompt());
        submit(&tx, "right");
        assert_eq!(rx.recv().unwrap(), Event::Started);
        worker.join().unwrap();
        mock.join().unwrap();
    }

    #[test]
    fn faillock_info_explains_the_failure() {
        let (tx, rx, mock, worker) = start(Lockout::Required);
        fail_twice(&tx, &rx);
        assert_eq!(rx.recv().unwrap(), msg(false, LOCKED));
        assert_eq!(rx.recv().unwrap(), msg(false, LEFT));
        assert_eq!(rx.recv().unwrap(), prompt());
        submit(&tx, "right");
        assert_eq!(rx.recv().unwrap(), Event::Failed(format!("{LOCKED} {LEFT}")));
        drop(tx);
        drop(rx);
        let _ = worker.join();
        let _ = mock.join();
    }

    #[test]
    fn lockout_without_prompt_does_not_spin() {
        let (tx, rx, mock, worker) = start(Lockout::Requisite);
        fail_twice(&tx, &rx);
        assert_eq!(rx.recv().unwrap(), msg(true, LOCKED));
        assert_eq!(rx.recv().unwrap(), msg(true, LEFT));
        assert_eq!(rx.recv().unwrap(), Event::Failed(format!("{LOCKED} {LEFT}")));
        // No new attempt until the user presses Enter again.
        assert!(rx.recv_timeout(std::time::Duration::from_millis(200)).is_err());
        submit(&tx, "right");
        assert_eq!(rx.recv().unwrap(), msg(true, LOCKED));
        assert_eq!(rx.recv().unwrap(), msg(true, LEFT));
        assert_eq!(rx.recv().unwrap(), Event::Failed(format!("{LOCKED} {LEFT}")));
        drop(tx);
        worker.join().unwrap();
        drop(rx);
        // The mock blocks reading the next request; the worker hung up, so it panics on EOF.
        assert!(mock.join().is_err());
    }

    #[test]
    fn messages_prefer_errors() {
        let mut m = Messages::default();
        assert_eq!(m.explanation(), None);
        m.push(false, "Place your finger on the reader");
        assert_eq!(m.explanation().as_deref(), Some("Place your finger on the reader"));
        m.push(true, "Failed to match fingerprint");
        m.push(true, "  ");
        assert_eq!(m.explanation().as_deref(), Some("Failed to match fingerprint"));
    }

    #[test]
    fn socket_loss_is_fatal() {
        let (a, b) = UnixStream::pair().unwrap();
        drop(b);
        let (_ctx, crx) = mpsc::channel();
        let (etx, erx) = mpsc::channel();
        run(a, "sigma", crx, move |e| etx.send(e).unwrap());
        assert!(matches!(erx.recv().unwrap(), Event::Fatal(_)));
    }
}

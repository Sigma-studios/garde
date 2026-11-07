//! What both modes show: wallpaper, and on one output a card with the user's name, the masked
//! password field, caps-lock warning and status line, with the keyboard layout in the bottom-left
//! corner (plus the session picker there and power buttons bottom-right at the login screen).
//! Pure state and geometry here; pixels in `draw`.

pub mod draw;
pub mod text;

use std::time::{Duration, Instant};

use zeroize::{Zeroize, Zeroizing};

use crate::logind::Power;

/// Bytes reserved for the password up front, so typing never reallocates and leaves copies of
/// it in freed memory. Input beyond this is ignored.
pub const PASSWORD_CAPACITY: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    Error,
    Busy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub text: String,
    pub tone: Tone,
}

/// Clock view (0) to input view (1). Moves linearly at a constant speed; each element eases its
/// own way from that (`draw`). Reversing mid-way turns back from where it is.
#[derive(Debug, Clone, Copy)]
pub struct Reveal {
    from: f32,
    to: f32,
    start: Instant,
}

/// A full clock-to-input (or back) transition.
pub const REVEAL: Duration = Duration::from_millis(480);

impl Reveal {
    pub fn new() -> Self {
        Self { from: 0.0, to: 0.0, start: Instant::now() }
    }

    pub fn value(&self, now: Instant) -> f32 {
        let span = (self.to - self.from).abs();
        if span == 0.0 {
            return self.to;
        }
        let p = (now.saturating_duration_since(self.start).as_secs_f32() / (REVEAL.as_secs_f32() * span)).min(1.0);
        self.from + (self.to - self.from) * p
    }

    /// Heading for the input view.
    pub fn input(&self) -> bool {
        self.to == 1.0
    }

    pub fn settled(&self, now: Instant) -> bool {
        self.value(now) == self.to
    }

    /// Head for the input view (`true`) or the clock. `false` when already heading there.
    pub fn set(&mut self, input: bool, now: Instant) -> bool {
        let to = if input { 1.0 } else { 0.0 };
        if self.to == to {
            return false;
        }
        *self = Self { from: self.value(now), to, start: now };
        true
    }
}

impl Default for Reveal {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Ui {
    pub name: String,
    /// Clock or input view, and the transition between them.
    pub reveal: Reveal,
    /// The clock view's time ("14:05") and date ("7 November 2026").
    pub clock: (String, String),
    password: Zeroizing<String>,
    /// Hidden prompt (password) or visible one (PAM asked for something echoed).
    pub masked: bool,
    pub placeholder: String,
    pub status: Option<Status>,
    pub caps: bool,
    pub layout: String,
    pub layouts_available: bool,
    /// Login screen only: the session picked, and whether there's more than one.
    pub session: Option<(String, bool)>,
    /// Login screen only: shut down and reboot buttons in the bottom-right corner.
    pub power_buttons: bool,
    pub busy: bool,
    pub caret: bool,
}

impl Ui {
    pub fn new(name: String) -> Self {
        Self {
            name,
            reveal: Reveal::new(),
            clock: clock_now(),
            password: Zeroizing::new(String::with_capacity(PASSWORD_CAPACITY)),
            masked: true,
            placeholder: "Password".into(),
            status: None,
            caps: false,
            layout: String::new(),
            layouts_available: false,
            session: None,
            power_buttons: false,
            busy: false,
            caret: true,
        }
    }

    pub fn password(&self) -> &str {
        &self.password
    }

    pub fn password_len(&self) -> usize {
        self.password.chars().count()
    }

    /// Append typed text. `false` (ignored) when it would overflow the reserved capacity.
    pub fn push(&mut self, s: &str) -> bool {
        if self.password.len() + s.len() > PASSWORD_CAPACITY {
            return false;
        }
        self.password.push_str(s);
        true
    }

    pub fn backspace(&mut self) {
        let Some(c) = self.password.chars().next_back() else { return };
        let keep = self.password.len() - c.len_utf8();
        // Wipe the removed bytes before shortening, so they don't linger past the end.
        unsafe {
            let v = self.password.as_mut_vec();
            v[keep..].zeroize();
            v.set_len(keep);
        }
    }

    pub fn clear(&mut self) {
        // Zeroizes the whole allocation, keeps the capacity.
        self.password.zeroize();
    }

    /// Move the password out for checking, wiping the field.
    pub fn take_password(&mut self) -> Zeroizing<String> {
        let mut out = Zeroizing::new(String::with_capacity(self.password.len()));
        out.push_str(&self.password);
        self.clear();
        out
    }

    pub fn set_status(&mut self, tone: Tone, text: impl Into<String>) {
        self.status = Some(Status { text: text.into(), tone });
    }

    /// Placeholder from a PAM prompt: "Password: " → "Password".
    pub fn set_prompt(&mut self, secret: bool, prompt: &str) {
        self.masked = secret;
        let p = prompt.trim().trim_end_matches(':').trim();
        self.placeholder = if p.is_empty() { if secret { "Password" } else { "Press Enter" }.into() } else { p.into() };
    }
}

/// Local time as ("14:05", "7 November 2026"), 24-hour.
pub fn clock_now() -> (String, String) {
    let t = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return (String::new(), String::new());
    }
    clock_text(tm.tm_hour, tm.tm_min, tm.tm_mday, tm.tm_mon, tm.tm_year + 1900)
}

fn clock_text(hour: i32, min: i32, mday: i32, mon: i32, year: i32) -> (String, String) {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let month = MONTHS.get(mon as usize).copied().unwrap_or("");
    (format!("{hour:02}:{min:02}"), format!("{mday} {month} {year}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
}

pub const CARD_W: f32 = 340.0;
pub const PAD: f32 = 26.0;
pub const NAME_SIZE: f32 = 22.0;
pub const NAME_H: f32 = 30.0;
pub const FIELD_H: f32 = 42.0;
pub const SMALL: f32 = 13.5;
pub const LINE: f32 = 19.0;
pub const STATUS_LINES: usize = 3;
pub const TIME_SIZE: f32 = 54.0;
pub const TIME_H: f32 = 70.0;
pub const DATE_H: f32 = 22.0;
/// The controls in the bottom corners: height (and power buttons' diameter), distance from the
/// screen's edges, gap between them; session picker and layout pill widths.
pub const CORNER_H: f32 = 40.0;
pub const CORNER_MARGIN: f32 = 28.0;
pub const CORNER_GAP: f32 = 12.0;
pub const SESSION_W: f32 = 210.0;
pub const LAYOUT_W: f32 = 52.0;
/// How far `ease_out_back` carries the controls past their place, at most.
pub const CORNER_OVERSHOOT: f32 = 12.0;

/// 0 → 1 overshooting a little before settling.
pub fn ease_out_back(p: f32) -> f32 {
    let c1 = 1.5;
    let c3 = c1 + 1.0;
    let q = p - 1.0;
    1.0 + c3 * q * q * q + c1 * q * q
}

/// 0 → 1, slow at both ends.
pub fn smooth(p: f32) -> f32 {
    let p = p.clamp(0.0, 1.0);
    p * p * (3.0 - 2.0 * p)
}

/// Card layout in logical px, relative to the card's top-left corner: the name, the password
/// field and, only while they're shown, the caps-lock warning and the status message. Messages
/// grow the card downwards.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub card: Rect,
    /// The card's height with no message, which fixes its position: messages never move the
    /// name and field.
    pub base_h: f32,
    pub name: Rect,
    pub field: Rect,
    /// Clock view: the time and the date, in the same space as the name and field.
    pub time: Rect,
    pub date: Rect,
    pub caps: Rect,
    pub status: Rect,
    pub status_lines: usize,
}

impl Geometry {
    /// `caps`: the warning is shown. `status_lines`: lines the status message wraps to (0 = none,
    /// at most `STATUS_LINES`).
    pub fn new(caps: bool, status_lines: usize) -> Self {
        let mut g = Self::layout(caps, status_lines.min(STATUS_LINES));
        g.base_h = Self::layout(false, 0).card.h;
        g
    }

    fn layout(caps_on: bool, status_lines: usize) -> Self {
        let inner = CARD_W - 2.0 * PAD;
        let mut y = PAD;
        let name = Rect { x: PAD, y, w: inner, h: NAME_H };
        y += NAME_H + 14.0;
        let field = Rect { x: PAD, y, w: inner, h: FIELD_H };
        y += FIELD_H;
        let clock_h = TIME_H + DATE_H;
        let time = Rect { x: PAD, y: (y + PAD - clock_h) / 2.0, w: inner, h: TIME_H };
        let date = Rect { x: PAD, y: time.y + TIME_H, w: inner, h: DATE_H };
        let mut m = y + 10.0;
        let caps = Rect { x: PAD, y: m, w: inner, h: if caps_on { LINE } else { 0.0 } };
        m += caps.h;
        let status = Rect { x: PAD, y: m, w: inner, h: LINE * status_lines as f32 };
        m += status.h;
        if caps_on || status_lines > 0 {
            y = m;
        }
        y += PAD;
        Self {
            card: Rect { x: 0.0, y: 0.0, w: CARD_W, h: y },
            base_h: y,
            name,
            field,
            time,
            date,
            caps,
            status,
            status_lines,
        }
    }

    /// Top-left of the card on a `w`×`h` (logical) output: centered, slightly above the middle,
    /// placed by its height without messages.
    pub fn origin(&self, w: f32, h: f32) -> (f32, f32) {
        let x = ((w - self.card.w) / 2.0).max(0.0).round();
        let y = ((h - self.base_h) / 2.0 - h * 0.04).max(0.0).min((h - self.card.h).max(0.0)).round();
        (x, y)
    }

    /// The login screen's power buttons on a `w`×`h` (logical) output, bottom-right:
    /// shut down in the corner, reboot to its left.
    pub fn power_buttons(w: f32, h: f32) -> [(Power, Rect); 2] {
        Self::power_at(w, h)
    }

    fn power_at(w: f32, h: f32) -> [(Power, Rect); 2] {
        let y = h - CORNER_MARGIN - CORNER_H;
        let off = Rect { x: w - CORNER_MARGIN - CORNER_H, y, w: CORNER_H, h: CORNER_H };
        let reboot = Rect { x: off.x - CORNER_GAP - CORNER_H, ..off };
        [(Power::PowerOff, off), (Power::Reboot, reboot)]
    }
}

/// The bottom-left corner of the output showing the card, in logical px relative to the output:
/// the session picker (login screen) and the keyboard layout.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Corner {
    /// Everything below, for redrawing.
    pub area: Rect,
    pub session: Option<Rect>,
    pub prev: Rect,
    pub next: Rect,
    pub layout: Rect,
}

impl Corner {
    pub fn new(login: bool, h: f32) -> Self {
        let y = h - CORNER_MARGIN - CORNER_H;
        let mut x = CORNER_MARGIN;
        let (session, prev, next) = if login {
            let s = Rect { x, y, w: SESSION_W, h: CORNER_H };
            x += SESSION_W + CORNER_GAP;
            let prev = Rect { x: s.x, y, w: CORNER_H, h: CORNER_H };
            let next = Rect { x: s.x + s.w - CORNER_H, ..prev };
            (Some(s), prev, next)
        } else {
            (None, Rect::default(), Rect::default())
        };
        let layout = Rect { x, y, w: LAYOUT_W, h: CORNER_H };
        // Down to the screen's edge, where the controls slide in from, plus room for the
        // overshoot above them.
        let (top, bottom) = (y - CORNER_OVERSHOOT, h);
        let area = Rect { x: CORNER_MARGIN, y: top, w: layout.x + layout.w - CORNER_MARGIN, h: bottom - top };
        Self { area, session, prev, next, layout }
    }

    /// The power buttons' area (login screen), like `area`, on a `w`×`h` output.
    pub fn power_area(w: f32, h: f32) -> Rect {
        let [(_, off), (_, reboot)] = Geometry::power_at(w, h);
        let top = off.y - CORNER_OVERSHOOT;
        Rect { x: reboot.x, y: top, w: off.x + off.w - reboot.x, h: h - top }
    }

    /// How far below their place the controls are at this point of the reveal: off the screen
    /// at 0, springing up past their place and settling at 1.
    pub fn drop(reveal: f32) -> f32 {
        let p = ((reveal - 0.12) / 0.88).clamp(0.0, 1.0);
        (1.0 - ease_out_back(p)) * (CORNER_MARGIN + CORNER_H + 4.0)
    }

    /// What a click at output-relative (x, y) lands on.
    pub fn hit(&self, x: f32, y: f32, ui: &Ui) -> Option<Hit> {
        if ui.layouts_available && self.layout.contains(x, y) {
            return Some(Hit::Layout);
        }
        if let (Some(s), Some((_, true))) = (self.session, &ui.session) {
            if self.prev.contains(x, y) {
                return Some(Hit::PrevSession);
            }
            if s.contains(x, y) {
                return Some(Hit::NextSession);
            }
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Layout,
    PrevSession,
    NextSession,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_editing_keeps_its_buffer() {
        let mut ui = Ui::new("Sigma".into());
        let cap = ui.password.capacity();
        assert!(cap >= PASSWORD_CAPACITY);
        assert!(ui.push("hé"));
        assert!(ui.push("llo"));
        ui.backspace();
        assert_eq!(ui.password(), "héll");
        for _ in 0..3 {
            ui.backspace();
        }
        assert_eq!(ui.password(), "h");
        // The bytes past the end were wiped.
        let spare = unsafe { std::slice::from_raw_parts(ui.password.as_ptr(), 4) };
        assert_eq!(spare, b"h\0\0\0");
        let taken = ui.take_password();
        assert_eq!(&*taken, "h");
        assert_eq!(ui.password(), "");
        assert_eq!(ui.password.capacity(), cap, "no reallocation");
        assert!(!ui.push(&"x".repeat(PASSWORD_CAPACITY + 1)));
        assert!(ui.push(&"x".repeat(PASSWORD_CAPACITY)));
        assert_eq!(ui.password.capacity(), cap);
    }

    #[test]
    fn prompts_become_placeholders() {
        let mut ui = Ui::new("x".into());
        ui.set_prompt(true, "Password: ");
        assert_eq!(ui.placeholder, "Password");
        ui.set_prompt(false, "Verification code:");
        assert!(!ui.masked);
        assert_eq!(ui.placeholder, "Verification code");
        ui.set_prompt(false, "");
        assert_eq!(ui.placeholder, "Press Enter");
    }

    #[test]
    fn corner_hits() {
        let c = Corner::new(true, 1080.0);
        let mut ui = Ui::new("x".into());
        let cx = |r: Rect| (r.x + r.w / 2.0, r.y + r.h / 2.0);
        let (px, py) = cx(c.layout);
        assert_eq!(c.hit(px, py, &ui), None, "single layout: the pill is inert");
        ui.layouts_available = true;
        assert_eq!(c.hit(px, py, &ui), Some(Hit::Layout));
        let (nx, ny) = cx(c.next);
        assert_eq!(c.hit(nx, ny, &ui), None, "one session: no picker");
        ui.session = Some(("COSMIC".into(), true));
        assert_eq!(c.hit(nx, ny, &ui), Some(Hit::NextSession));
        let (sx, sy) = cx(c.session.unwrap());
        assert_eq!(c.hit(sx, sy, &ui), Some(Hit::NextSession), "the name also picks the next one");
        let (vx, vy) = cx(c.prev);
        assert_eq!(c.hit(vx, vy, &ui), Some(Hit::PrevSession));
        assert_eq!(c.area.y + c.area.h, 1080.0, "down to the edge, where the controls slide in from");
        assert_eq!(c.layout.y + c.layout.h, 1080.0 - CORNER_MARGIN);
        assert!(c.layout.x > c.next.x + c.next.w);
        let lock = Corner::new(false, 1080.0);
        assert_eq!(lock.session, None);
        assert_eq!(lock.layout.x, CORNER_MARGIN);
        let g = Geometry::new(false, 0);
        assert_eq!(g.origin(2560.0, 1440.0).0, 1110.0);
        assert_eq!(g.origin(100.0, 100.0), (0.0, 0.0));
    }

    #[test]
    fn messages_grow_the_card_downwards() {
        let plain = Geometry::new(false, 0);
        let busy = Geometry::new(true, 2);
        assert_eq!(busy.card.h, plain.card.h + 10.0 + 3.0 * LINE);
        assert_eq!(busy.base_h, plain.card.h);
        assert_eq!(busy.origin(2560.0, 1440.0), plain.origin(2560.0, 1440.0), "name and field stay put");
        assert_eq!(busy.field, plain.field);
        assert!(busy.status.y >= busy.caps.y + LINE);
        assert_eq!(Geometry::new(false, 9).status_lines, STATUS_LINES);
        // A short output: the grown card is pulled up to stay on screen.
        let (_, y) = busy.origin(800.0, busy.card.h + 4.0);
        assert!(y + busy.card.h <= busy.card.h + 4.0);
    }

    #[test]
    fn reveal_runs_and_turns_back() {
        let t0 = Instant::now();
        let mut r = Reveal::new();
        assert_eq!(r.value(t0), 0.0);
        assert!(r.set(true, t0));
        assert!(!r.set(true, t0), "already heading there");
        let half = t0 + REVEAL / 2;
        assert!((r.value(half) - 0.5).abs() < 0.01);
        // Back from half-way: it takes half the time.
        assert!(r.set(false, half));
        assert!((r.value(half) - 0.5).abs() < 0.01);
        assert!(r.settled(half + REVEAL / 2 + Duration::from_millis(1)));
        assert_eq!(r.value(half + REVEAL), 0.0);
    }

    #[test]
    fn controls_spring_up() {
        assert!(Corner::drop(0.0) >= CORNER_MARGIN + CORNER_H, "off screen");
        assert_eq!(Corner::drop(1.0), 0.0);
        let lowest = (0..=100).map(|i| Corner::drop(i as f32 / 100.0)).fold(f32::MAX, f32::min);
        assert!(lowest < 0.0 && lowest > -CORNER_OVERSHOOT, "overshoots, within the redrawn area: {lowest}");
        let a = Corner::power_area(1920.0, 1080.0);
        assert_eq!(a.y + a.h, 1080.0);
        assert_eq!(a.x + a.w, 1920.0 - CORNER_MARGIN);
    }

    #[test]
    fn clock_text_is_24_hour() {
        assert_eq!(clock_text(9, 5, 7, 10, 2026), ("09:05".into(), "7 November 2026".into()));
        assert_eq!(clock_text(23, 59, 31, 11, 2026).0, "23:59");
    }

    #[test]
    fn power_buttons_sit_in_the_corner() {
        let [(a, off), (b, reboot)] = Geometry::power_buttons(1920.0, 1080.0);
        assert_eq!((a, b), (Power::PowerOff, Power::Reboot));
        assert_eq!(off.x + off.w, 1920.0 - CORNER_MARGIN);
        assert_eq!(off.y + off.h, 1080.0 - CORNER_MARGIN);
        assert!(reboot.x + reboot.w < off.x);
    }
}

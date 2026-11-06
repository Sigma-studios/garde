//! What both modes show: wallpaper, and on one output a card with the user's name, the masked
//! password field, caps-lock warning, layout indicator and status line (plus the session picker
//! and power hints at the login screen). Pure state and geometry here; pixels in `draw`.

pub mod draw;
pub mod text;

use zeroize::{Zeroize, Zeroizing};

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

pub struct Ui {
    pub name: String,
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
    /// Login screen only: bottom-of-screen key hints.
    pub hints: Option<String>,
    pub busy: bool,
    pub caret: bool,
}

impl Ui {
    pub fn new(name: String) -> Self {
        Self {
            name,
            password: Zeroizing::new(String::with_capacity(PASSWORD_CAPACITY)),
            masked: true,
            placeholder: "Password".into(),
            status: None,
            caps: false,
            layout: String::new(),
            layouts_available: false,
            session: None,
            hints: None,
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

pub const CARD_W: f32 = 400.0;
pub const PAD: f32 = 32.0;
pub const NAME_SIZE: f32 = 26.0;
pub const NAME_H: f32 = 36.0;
pub const FIELD_H: f32 = 46.0;
pub const SMALL: f32 = 13.5;
pub const LINE: f32 = 19.0;
pub const STATUS_LINES: usize = 3;
pub const SESSION_H: f32 = 34.0;

/// Card layout in logical px, relative to the card's top-left corner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub card: Rect,
    pub name: Rect,
    pub field: Rect,
    pub layout_pill: Rect,
    pub caps: Rect,
    pub status: Rect,
    pub session: Option<Rect>,
    pub prev: Rect,
    pub next: Rect,
}

impl Geometry {
    pub fn new(login: bool) -> Self {
        let inner = CARD_W - 2.0 * PAD;
        let mut y = PAD;
        let name = Rect { x: PAD, y, w: inner, h: NAME_H };
        y += NAME_H + 20.0;
        let field = Rect { x: PAD, y, w: inner, h: FIELD_H };
        let pill_h = 28.0;
        let pill_w = 44.0;
        let layout_pill = Rect { x: field.x + field.w - pill_w - 9.0, y: field.y + (FIELD_H - pill_h) / 2.0, w: pill_w, h: pill_h };
        y += FIELD_H + 12.0;
        let caps = Rect { x: PAD, y, w: inner, h: LINE };
        y += LINE + 2.0;
        let status = Rect { x: PAD, y, w: inner, h: LINE * STATUS_LINES as f32 };
        y += status.h;
        let (session, prev, next) = if login {
            y += 8.0;
            let s = Rect { x: PAD, y, w: inner, h: SESSION_H };
            y += SESSION_H;
            let prev = Rect { x: s.x, y: s.y, w: SESSION_H + 8.0, h: SESSION_H };
            let next = Rect { x: s.x + s.w - SESSION_H - 8.0, ..prev };
            (Some(s), prev, next)
        } else {
            (None, Rect::default(), Rect::default())
        };
        y += PAD - 6.0;
        Self { card: Rect { x: 0.0, y: 0.0, w: CARD_W, h: y }, name, field, layout_pill, caps, status, session, prev, next }
    }

    /// Top-left of the card centered on a `w`×`h` (logical) output, slightly above the middle.
    pub fn origin(&self, w: f32, h: f32) -> (f32, f32) {
        let x = ((w - self.card.w) / 2.0).max(0.0).round();
        let y = ((h - self.card.h) / 2.0 - h * 0.04).max(0.0).round();
        (x, y)
    }

    /// What a click at card-relative (x, y) lands on.
    pub fn hit(&self, x: f32, y: f32, ui: &Ui) -> Option<Hit> {
        if ui.layouts_available && self.layout_pill.contains(x, y) {
            return Some(Hit::Layout);
        }
        if let (Some(_), Some((_, true))) = (self.session, &ui.session) {
            if self.prev.contains(x, y) {
                return Some(Hit::PrevSession);
            }
            if self.next.contains(x, y) {
                return Some(Hit::NextSession);
            }
        }
        if self.field.contains(x, y) {
            return Some(Hit::Field);
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Layout,
    PrevSession,
    NextSession,
    Field,
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
    fn geometry_and_hits() {
        let g = Geometry::new(true);
        let mut ui = Ui::new("x".into());
        let cx = |r: Rect| (r.x + r.w / 2.0, r.y + r.h / 2.0);
        let (px, py) = cx(g.layout_pill);
        assert_eq!(g.hit(px, py, &ui), Some(Hit::Field), "single layout: the pill is inert");
        ui.layouts_available = true;
        assert_eq!(g.hit(px, py, &ui), Some(Hit::Layout));
        let (nx, ny) = cx(g.next);
        assert_eq!(g.hit(nx, ny, &ui), None);
        ui.session = Some(("COSMIC".into(), true));
        assert_eq!(g.hit(nx, ny, &ui), Some(Hit::NextSession));
        let (vx, vy) = cx(g.prev);
        assert_eq!(g.hit(vx, vy, &ui), Some(Hit::PrevSession));
        assert!(Geometry::new(false).card.h < g.card.h);
        assert_eq!(g.origin(2560.0, 1440.0).0, 1080.0);
        assert_eq!(g.origin(100.0, 100.0), (0.0, 0.0));
    }
}

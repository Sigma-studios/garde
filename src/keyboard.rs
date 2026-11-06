//! Keyboard state on top of the compositor's keymap.
//!
//! The compositor owns the keymap (cage gets `fr,us` from XKB_DEFAULT_LAYOUT, the desktop uses the
//! user's layouts) and the modifier state. garde only overrides two things locally, so they work
//! even where the compositor offers no way to change them:
//! - the active layout (group), cycled with Super+Alt+K or a click, starting on the first one;
//! - num lock, forced on at start when `numlock = true`.
//!
//! Caps lock is read from the modifier state the compositor sends, never guessed from key events.

use std::os::fd::OwnedFd;

use xkbcommon::xkb;

/// Which layout is active: the compositor's, unless the user cycled it here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutTracker {
    count: u32,
    /// Local choice, overriding the compositor's group until the compositor changes its own.
    chosen: Option<u32>,
    last_compositor: Option<u32>,
}

impl LayoutTracker {
    /// Start on the first layout, whatever the compositor had.
    pub fn new() -> Self {
        Self { count: 1, chosen: Some(0), last_compositor: None }
    }

    pub fn set_count(&mut self, n: u32) {
        self.count = n.max(1);
        if let Some(c) = &mut self.chosen {
            *c %= self.count;
        }
    }

    /// The compositor's group from a modifiers event. A change there (the user switched layouts
    /// with a compositor shortcut) wins over the local choice; a repeat of the same value doesn't.
    pub fn compositor_group(&mut self, g: u32) {
        if self.last_compositor.is_some_and(|last| last != g) {
            self.chosen = None;
        }
        self.last_compositor = Some(g);
    }

    pub fn active(&self) -> u32 {
        self.chosen.or(self.last_compositor).unwrap_or(0) % self.count
    }

    pub fn cycle(&mut self) {
        self.chosen = Some((self.active() + 1) % self.count);
    }
}

impl Default for LayoutTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Short label for a layout: the configured code when the keymap's layouts line up with the
/// config, else something derived from the keymap's own (long) name.
pub fn layout_label(configured: &[String], keymap_count: u32, index: u32, keymap_name: &str) -> String {
    if configured.len() as u32 == keymap_count {
        if let Some(code) = configured.get(index as usize) {
            return code.split(['(', ':']).next().unwrap_or(code).to_string();
        }
    }
    // Unnamed (virtual keyboards upload ad-hoc keymaps): nothing worth showing.
    keymap_name.trim().to_string()
}

/// Num lock forced on locally: XOR the compositor's num lock bit, decided once from the first
/// state seen, so pressing Num Lock still toggles it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NumLock {
    want_on: bool,
    flip: Option<bool>,
}

impl NumLock {
    pub fn new(want_on: bool) -> Self {
        Self { want_on, flip: None }
    }

    /// Before the compositor's first modifiers event: assume num lock off there.
    pub fn initial(&self, locked: u32, num_mask: u32) -> u32 {
        if self.want_on {
            locked | num_mask
        } else {
            locked
        }
    }

    pub fn apply(&mut self, locked: u32, num_mask: u32) -> u32 {
        let on = locked & num_mask != 0;
        let flip = *self.flip.get_or_insert(self.want_on && !on);
        if flip {
            locked ^ num_mask
        } else {
            locked
        }
    }
}

/// What a key press means for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Text(String),
    Backspace,
    Clear,
    Submit,
    CycleLayout,
    Left,
    Right,
    Escape,
    F(u8),
}

pub struct Keyboard {
    ctx: xkb::Context,
    keymap: Option<xkb::Keymap>,
    state: Option<xkb::State>,
    pub layouts: LayoutTracker,
    numlock: NumLock,
    mods: (u32, u32, u32),
    mods_seen: bool,
    pub repeat: Option<(u32, u32)>,
}

impl Keyboard {
    pub fn new(numlock: bool) -> Self {
        Self {
            ctx: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            keymap: None,
            state: None,
            layouts: LayoutTracker::new(),
            numlock: NumLock::new(numlock),
            mods: (0, 0, 0),
            mods_seen: false,
            repeat: Some((25, 600)),
        }
    }

    /// wl_keyboard.keymap (xkb v1 format).
    pub fn set_keymap_fd(&mut self, fd: OwnedFd, size: u32) {
        let km = unsafe {
            xkb::Keymap::new_from_fd(
                &self.ctx,
                fd,
                size as usize,
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        };
        match km {
            Ok(Some(km)) => self.set_keymap(km),
            Ok(None) => log::error!("compositor sent an invalid keymap"),
            Err(e) => log::error!("reading keymap: {e}"),
        }
    }

    pub fn set_keymap(&mut self, km: xkb::Keymap) {
        self.layouts.set_count(km.num_layouts());
        self.state = Some(xkb::State::new(&km));
        self.keymap = Some(km);
        self.apply();
    }

    /// wl_keyboard.modifiers.
    pub fn set_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        self.layouts.compositor_group(group);
        self.mods = (depressed, latched, locked);
        self.mods_seen = true;
        self.apply();
    }

    fn mod_mask(&self, name: &str) -> u32 {
        let Some(km) = &self.keymap else { return 0 };
        let i = km.mod_get_index(name);
        if i == xkb::MOD_INVALID {
            0
        } else {
            1 << i
        }
    }

    fn apply(&mut self) {
        let num = self.mod_mask(xkb::MOD_NAME_NUM);
        let (d, l, lk) = self.mods;
        let locked = match (num, self.mods_seen) {
            (0, _) => lk,
            (_, false) => self.numlock.initial(lk, num),
            (_, true) => self.numlock.apply(lk, num),
        };
        let group = self.layouts.active();
        if let Some(st) = &mut self.state {
            st.update_mask(d, l, locked, 0, 0, group);
        }
    }

    pub fn cycle_layout(&mut self) {
        self.layouts.cycle();
        self.apply();
    }

    pub fn caps_lock(&self) -> bool {
        self.state.as_ref().is_some_and(|s| s.mod_name_is_active(xkb::MOD_NAME_CAPS, xkb::STATE_MODS_EFFECTIVE))
    }

    #[cfg(test)]
    pub fn num_lock(&self) -> bool {
        self.state.as_ref().is_some_and(|s| s.mod_name_is_active(xkb::MOD_NAME_NUM, xkb::STATE_MODS_EFFECTIVE))
    }

    fn active(&self, name: &str) -> bool {
        self.state.as_ref().is_some_and(|s| s.mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE))
    }

    pub fn layout_count(&self) -> u32 {
        self.keymap.as_ref().map_or(1, |k| k.num_layouts())
    }

    pub fn layout_name(&self, i: u32) -> String {
        self.keymap.as_ref().map(|k| k.layout_get_name(i).to_string()).unwrap_or_default()
    }

    pub fn key_repeats(&self, evdev: u32) -> bool {
        self.keymap.as_ref().is_some_and(|k| k.key_repeats((evdev + 8).into()))
    }

    /// Translate an evdev key press.
    pub fn press(&self, evdev: u32) -> Option<Action> {
        let st = self.state.as_ref()?;
        let code: xkb::Keycode = (evdev + 8).into();
        let sym = st.key_get_one_sym(code);
        let ctrl = self.active(xkb::MOD_NAME_CTRL);
        let alt = self.active(xkb::MOD_NAME_ALT);
        let logo = self.active(xkb::MOD_NAME_LOGO);
        use xkb::Keysym as K;
        let a = match sym {
            K::Return | K::KP_Enter | K::ISO_Enter => Action::Submit,
            K::BackSpace if ctrl => Action::Clear,
            K::BackSpace => Action::Backspace,
            K::Escape => Action::Escape,
            K::Left => Action::Left,
            K::Right => Action::Right,
            s if s.raw() >= K::F1.raw() && s.raw() <= K::F12.raw() => Action::F((s.raw() - K::F1.raw() + 1) as u8),
            _ => {
                // Super+Alt+K. Check the key's symbol in every layout so it works on any of them.
                if logo && alt && self.is_k(code) {
                    return Some(Action::CycleLayout);
                }
                if ctrl && self.base_sym(code) == Some(K::u) {
                    return Some(Action::Clear);
                }
                if ctrl || alt || logo {
                    return None;
                }
                let s = st.key_get_utf8(code);
                if s.is_empty() || s.chars().any(char::is_control) {
                    return None;
                }
                Action::Text(s)
            }
        };
        Some(a)
    }

    fn base_sym(&self, code: xkb::Keycode) -> Option<xkb::Keysym> {
        let km = self.keymap.as_ref()?;
        let g = self.layouts.active();
        km.key_get_syms_by_level(code, g, 0).first().copied()
    }

    fn is_k(&self, code: xkb::Keycode) -> bool {
        let Some(km) = &self.keymap else { return false };
        (0..km.num_layouts())
            .any(|g| km.key_get_syms_by_level(code, g, 0).iter().any(|s| *s == xkb::Keysym::k || *s == xkb::Keysym::K))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_cycling() {
        let mut t = LayoutTracker::new();
        t.set_count(2);
        // The compositor starts on "us" (1): garde still starts on the first layout.
        t.compositor_group(1);
        assert_eq!(t.active(), 0);
        t.cycle();
        assert_eq!(t.active(), 1);
        t.cycle();
        assert_eq!(t.active(), 0);
        // Same compositor group repeated (every modifiers event): keep the local choice.
        t.compositor_group(1);
        assert_eq!(t.active(), 0);
        // The compositor switched layouts itself: follow it.
        t.compositor_group(0);
        assert_eq!(t.active(), 0);
        t.compositor_group(1);
        assert_eq!(t.active(), 1);
        // A keymap with fewer layouts clamps the choice.
        t.cycle();
        t.set_count(1);
        assert_eq!(t.active(), 0);
        t.cycle();
        assert_eq!(t.active(), 0);
    }

    #[test]
    fn labels() {
        let cfg = vec!["fr".to_string(), "us(intl)".to_string()];
        assert_eq!(layout_label(&cfg, 2, 0, "French"), "fr");
        assert_eq!(layout_label(&cfg, 2, 1, "English (US, intl.)"), "us");
        assert_eq!(layout_label(&cfg, 1, 0, "English (US)"), "English (US)");
        assert_eq!(layout_label(&[], 1, 0, ""), "");
    }

    #[test]
    fn numlock_xor() {
        let num = 1 << 4;
        let mut n = NumLock::new(true);
        assert_eq!(n.apply(0, num), num, "forced on at start");
        assert_eq!(n.apply(num, num), 0, "Num Lock pressed: compositor turns it on, we toggle off");
        let mut n = NumLock::new(true);
        assert_eq!(n.apply(num, num), num, "already on: leave alone");
        assert_eq!(n.apply(0, num), 0);
        let mut n = NumLock::new(false);
        assert_eq!(n.apply(0, num), 0);
    }

    fn keymap(layouts: &str) -> Option<Keyboard> {
        let mut kb = Keyboard::new(true);
        let km =
            xkb::Keymap::new_from_names(&kb.ctx, "evdev", "pc105", layouts, "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)?;
        kb.set_keymap(km);
        Some(kb)
    }

    // evdev codes
    const KEY_A: u32 = 30;
    const KEY_Q: u32 = 16;
    const KEY_K: u32 = 37;
    const KEY_1: u32 = 2;
    const KEY_KP1: u32 = 79;

    #[test]
    fn real_keymap_caps_layouts_numlock() {
        let Some(mut kb) = keymap("fr,us") else {
            eprintln!("no xkeyboard-config data (set XKB_CONFIG_ROOT); skipping");
            return;
        };
        let caps = kb.mod_mask(xkb::MOD_NAME_CAPS);
        let shift = kb.mod_mask(xkb::MOD_NAME_SHIFT);
        let logo = kb.mod_mask(xkb::MOD_NAME_LOGO);
        let alt = kb.mod_mask(xkb::MOD_NAME_ALT);
        assert_eq!(kb.layout_count(), 2);
        kb.set_modifiers(0, 0, 0, 0);
        assert!(!kb.caps_lock());
        assert!(kb.num_lock(), "numlock = true forces it on");
        // AZERTY: the key at QWERTY's Q types 'a', the top-row 1 types '&'.
        assert_eq!(kb.press(KEY_Q), Some(Action::Text("a".into())));
        assert_eq!(kb.press(KEY_1), Some(Action::Text("&".into())));
        assert_eq!(kb.press(KEY_KP1), Some(Action::Text("1".into())));

        // Caps lock comes from the locked modifier state.
        kb.set_modifiers(0, 0, caps, 0);
        assert!(kb.caps_lock());
        assert_eq!(kb.press(KEY_Q), Some(Action::Text("A".into())));
        kb.set_modifiers(shift, 0, 0, 0);
        assert!(!kb.caps_lock(), "Shift held is not caps lock");

        // Super+Alt+K cycles to "us".
        kb.set_modifiers(logo | alt, 0, 0, 0);
        assert_eq!(kb.press(KEY_K), Some(Action::CycleLayout));
        kb.cycle_layout();
        kb.set_modifiers(0, 0, 0, 0);
        assert_eq!(kb.layouts.active(), 1);
        assert_eq!(kb.press(KEY_Q), Some(Action::Text("q".into())));
        assert_eq!(kb.press(KEY_A), Some(Action::Text("a".into())));
        assert_eq!(kb.layout_name(1), "English (US)");
    }
}

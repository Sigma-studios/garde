//! The Wayland client shared by both modes: outputs, surfaces, input, drawing. Mode-specific
//! behavior (greetd at the login screen, PAM + session lock for the locker) lives in `login.rs`
//! and `lock.rs` and plugs in through `Mode`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry, delegate_seat,
    delegate_session_lock, delegate_shm, delegate_xdg_shell, delegate_xdg_window,
    output::{OutputHandler, OutputState},
    reexports::calloop::{
        timer::{TimeoutAction, Timer},
        LoopHandle, LoopSignal, RegistrationToken,
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        pointer::{CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec, ThemedPointer},
        Capability, SeatHandler, SeatState,
    },
    session_lock::{
        SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface, SessionLockSurfaceConfigure,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
        },
        xdg::{
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
            XdgShell,
        },
        WaylandSurface,
    },
    shm::{raw::RawPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::GlobalList,
    protocol::{wl_buffer, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};

use crate::config::Config;
use crate::keyboard::{layout_label, Action, Keyboard};
use crate::ui::draw::{blit, Look, Scene, Wallpaper};
use crate::ui::text::{Painter, Style};
use crate::ui::{Corner, Geometry, Hit, Ui};

const BTN_LEFT: u32 = 0x110;
const BLINK: Duration = Duration::from_millis(530);
/// Back to the clock after this long without input, with nothing typed.
const IDLE: Duration = Duration::from_secs(30);
/// Transition frame interval: about 144 Hz.
const FRAME: Duration = Duration::from_millis(7);

pub enum SurfKind {
    Lock(SessionLockSurface),
    Layer(LayerSurface),
    Window(Window),
}

impl SurfKind {
    fn wl_surface(&self) -> &wl_surface::WlSurface {
        match self {
            SurfKind::Lock(s) => s.wl_surface(),
            SurfKind::Layer(s) => s.wl_surface(),
            SurfKind::Window(s) => s.wl_surface(),
        }
    }
}

struct Buf {
    pool: RawPool,
    buffer: wl_buffer::WlBuffer,
    busy: Arc<AtomicBool>,
    size: (u32, u32),
    bg_valid: bool,
}

impl Drop for Buf {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

pub struct OutSurface {
    pub output: Option<wl_output::WlOutput>,
    pub kind: SurfKind,
    size: (u32, u32),
    scale: i32,
    configured: bool,
    scene: Option<Scene>,
    bufs: Vec<Buf>,
    dirty: bool,
}

impl OutSurface {
    pub fn new(output: Option<wl_output::WlOutput>, kind: SurfKind) -> Self {
        Self { output, kind, size: (0, 0), scale: 1, configured: false, scene: None, bufs: Vec::new(), dirty: true }
    }

    pub fn wl_surface(&self) -> &wl_surface::WlSurface {
        self.kind.wl_surface()
    }
}

pub struct App {
    pub conn: Connection,
    pub qh: QueueHandle<App>,
    pub handle: LoopHandle<'static, App>,
    pub signal: LoopSignal,
    registry: RegistryState,
    pub outputs: OutputState,
    pub compositor: CompositorState,
    shm: Shm,
    seat: SeatState,
    pub layer_shell: Option<LayerShell>,
    pub xdg_shell: Option<XdgShell>,
    pub lock_state: Option<SessionLockState>,

    pub cfg: Config,
    pub ui: Ui,
    geom: Geometry,
    look: Look,
    painter: Painter,
    wallpaper: Wallpaper,
    kb: Keyboard,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<ThemedPointer>,
    pub surfaces: Vec<OutSurface>,
    card_surface: Option<wayland_client::backend::ObjectId>,
    drawn_card: Option<wayland_client::backend::ObjectId>,
    repeat: Option<(u32, RegistrationToken)>,
    blink: Option<RegistrationToken>,
    /// Once a second while shown: the clock, and the idle return to it.
    tick: Option<RegistrationToken>,
    /// Frames while the clock/input transition runs.
    anim: Option<RegistrationToken>,
    last_input: Instant,

    pub mode: ModeState,
    pub exit_code: Option<i32>,
}

pub enum ModeState {
    Login(crate::login::Login),
    Lock(crate::lock::Lock),
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        conn: Connection,
        globals: &GlobalList,
        qh: QueueHandle<App>,
        handle: LoopHandle<'static, App>,
        signal: LoopSignal,
        cfg: Config,
        login: bool,
        mode: ModeState,
    ) -> Result<Self, String> {
        let compositor = CompositorState::bind(globals, &qh).map_err(|e| format!("wl_compositor: {e}"))?;
        let shm = Shm::bind(globals, &qh).map_err(|e| format!("wl_shm: {e}"))?;
        let name = crate::user::display_name(&cfg.user);
        let mut ui = Ui::new(name);
        ui.layout = cfg.layouts.first().cloned().unwrap_or_default();
        let look = Look { dim: cfg.dim, blur: cfg.blur };
        let painter = Painter::new(&cfg.font);
        let wallpaper = Wallpaper::load(cfg.wallpaper.as_deref());
        Ok(Self {
            registry: RegistryState::new(globals),
            outputs: OutputState::new(globals, &qh),
            seat: SeatState::new(globals, &qh),
            layer_shell: LayerShell::bind(globals, &qh).ok(),
            xdg_shell: XdgShell::bind(globals, &qh).ok(),
            lock_state: if login { None } else { Some(SessionLockState::new(globals, &qh)) },
            compositor,
            shm,
            conn,
            qh,
            handle,
            signal,
            kb: Keyboard::new(cfg.numlock),
            cfg,
            ui,
            geom: Geometry::new(false, 0),
            look,
            painter,
            wallpaper,
            keyboard: None,
            pointer: None,
            surfaces: Vec::new(),
            card_surface: None,
            drawn_card: None,
            repeat: None,
            blink: None,
            tick: None,
            anim: None,
            last_input: Instant::now(),
            mode,
            exit_code: None,
        })
    }

    pub fn exit(&mut self, code: i32) {
        self.exit_code = Some(code);
        self.signal.stop();
        self.signal.wakeup();
    }

    // ---- drawing -------------------------------------------------------------------------

    /// Index of the surface showing the card: the one under the pointer, else the first output
    /// (top-left in the compositor's layout).
    fn card_index(&self) -> Option<usize> {
        let by_pointer =
            self.card_surface.as_ref().and_then(|id| self.surfaces.iter().position(|s| &s.wl_surface().id() == id));
        by_pointer.or_else(|| {
            let pos = |s: &OutSurface| {
                s.output
                    .as_ref()
                    .and_then(|o| self.outputs.info(o))
                    .and_then(|i| i.logical_position)
                    .map(|(x, y)| (y, x))
            };
            (0..self.surfaces.len())
                .filter(|&i| self.surfaces[i].configured)
                .min_by_key(|&i| pos(&self.surfaces[i]).unwrap_or((i32::MAX, i as i32)))
        })
    }

    /// The card changed: redraw it (only its output).
    pub fn redraw_card(&mut self) {
        if let Some(i) = self.card_index() {
            self.surfaces[i].dirty = true;
        }
    }

    /// Draw every dirty surface. Called once per event-loop iteration, so bursts of events
    /// produce a single frame.
    pub fn flush(&mut self) {
        self.sync_geometry();
        let card = self.card_index();
        let card_id = card.map(|i| self.surfaces[i].wl_surface().id());
        if card_id != self.drawn_card {
            // The card moved: repaint where it was and where it goes.
            if let Some(old) =
                self.drawn_card.take().and_then(|id| self.surfaces.iter().position(|s| s.wl_surface().id() == id))
            {
                self.surfaces[old].dirty = true;
            }
            if let Some(i) = card {
                self.surfaces[i].dirty = true;
            }
            self.drawn_card = card_id;
        }
        for i in 0..self.surfaces.len() {
            if self.surfaces[i].dirty && self.surfaces[i].configured {
                self.draw(i, card == Some(i));
            }
        }
        let _ = self.conn.flush();
    }

    /// Fit the card to what it shows (caps-lock warning, status lines). When its size changes,
    /// every output is repainted in full: the old card may have covered more of it.
    fn sync_geometry(&mut self) {
        let lines = self.ui.status.as_ref().map_or(0, |st| {
            let style = Style { max_lines: crate::ui::STATUS_LINES, ..Style::new(crate::ui::SMALL, [1.0; 4]) };
            self.painter.line_count(&st.text, &style, self.geom.status.w)
        });
        let geom = Geometry::new(self.ui.caps && self.ui.reveal.input(), lines);
        if geom == self.geom {
            return;
        }
        self.geom = geom;
        for s in &mut self.surfaces {
            if let Some(scene) = &mut s.scene {
                scene.relayout(&self.geom, &self.look);
            }
            for b in &mut s.bufs {
                b.bg_valid = false;
            }
            s.dirty = true;
        }
    }

    fn draw(&mut self, i: usize, with_card: bool) {
        let s = &mut self.surfaces[i];
        let scale = s.scale.max(1);
        let (w, h) = (s.size.0 * scale as u32, s.size.1 * scale as u32);
        if w == 0 || h == 0 {
            return;
        }
        if s.scene.as_ref().is_none_or(|sc| (sc.width, sc.height, sc.scale) != (w, h, scale as f32)) {
            let t = std::time::Instant::now();
            s.scene =
                Some(Scene::build(&self.wallpaper, w, h, scale as f32, &self.geom, &self.look, self.ui.power_buttons));
            log::debug!("scene {w}x{h}@{scale} built in {:?}", t.elapsed());
            s.bufs.retain(|b| b.size == (w, h));
        }
        // A buffer the compositor isn't reading, else a new one (at most two per surface).
        let idx = match s.bufs.iter().position(|b| !b.busy.load(Ordering::Acquire)) {
            Some(idx) => idx,
            None if s.bufs.len() < 2 => {
                let stride = w * 4;
                let len = (stride * h) as usize;
                let mut pool = match RawPool::new(len, &self.shm) {
                    Ok(p) => p,
                    Err(e) => {
                        log::error!("shm pool: {e}");
                        return;
                    }
                };
                let busy = Arc::new(AtomicBool::new(false));
                let buffer = pool.create_buffer(
                    0,
                    w as i32,
                    h as i32,
                    stride as i32,
                    wl_shm::Format::Xrgb8888,
                    busy.clone(),
                    &self.qh,
                );
                s.bufs.push(Buf { pool, buffer, busy, size: (w, h), bg_valid: false });
                s.bufs.len() - 1
            }
            // Both in use: redraw when one is released.
            None => return,
        };
        let scene = s.scene.as_ref().unwrap();
        let buf = &mut s.bufs[idx];
        let canvas: &mut [u8] = buf.pool.mmap();
        let full = !buf.bg_valid;
        if full {
            canvas.copy_from_slice(&scene.bg_xrgb);
            buf.bg_valid = true;
        }
        let reveal = self.ui.reveal.value(Instant::now());
        let pm = if with_card {
            scene.render_card(&self.geom, &self.ui, reveal, &mut self.painter)
        } else {
            scene.plain_card()
        };
        let c = scene.card;
        blit(canvas, (w * 4) as usize, &pm, c.x, c.y);
        // The corner controls only exist once the input view starts showing.
        let shown = with_card && reveal > 0.0;
        let drop = Corner::drop(reveal);
        let corner = scene.render_corner(shown.then_some(&self.ui), drop, &mut self.painter);
        let k = scene.corner_px;
        blit(canvas, (w * 4) as usize, &corner, k.x, k.y);
        let power = scene.render_power(shown, drop);
        if let Some((r, pm)) = &power {
            blit(canvas, (w * 4) as usize, pm, r.x, r.y);
        }

        let surf = s.kind.wl_surface();
        surf.set_buffer_scale(scale);
        surf.attach(Some(&buf.buffer), 0, 0);
        if full {
            surf.damage_buffer(0, 0, w as i32, h as i32);
        } else {
            surf.damage_buffer(c.x as i32, c.y as i32, c.w as i32, c.h as i32);
            surf.damage_buffer(k.x as i32, k.y as i32, k.w as i32, k.h as i32);
            if let Some((r, _)) = power {
                surf.damage_buffer(r.x as i32, r.y as i32, r.w as i32, r.h as i32);
            }
        }
        buf.busy.store(true, Ordering::Release);
        surf.commit();
        s.dirty = false;
    }

    // ---- surfaces ------------------------------------------------------------------------

    /// Login screen: one overlay layer surface per output where layer-shell exists, else a
    /// single fullscreen xdg toplevel (cage).
    pub fn add_login_surface(&mut self, output: Option<wl_output::WlOutput>) {
        let surface = self.compositor.create_surface(&self.qh);
        let kind = if let (Some(ls), Some(out)) = (&self.layer_shell, &output) {
            let layer = ls.create_layer_surface(&self.qh, surface, Layer::Overlay, Some("garde"), Some(out));
            layer.set_anchor(Anchor::all());
            layer.set_exclusive_zone(-1);
            layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
            layer.commit();
            log::info!("login surface: layer-shell overlay");
            SurfKind::Layer(layer)
        } else if let Some(xdg) = &self.xdg_shell {
            let win = xdg.create_window(surface, WindowDecorations::None, &self.qh);
            win.set_title("garde");
            win.set_app_id("garde");
            win.set_fullscreen(output.as_ref());
            win.commit();
            log::info!("login surface: fullscreen xdg toplevel");
            SurfKind::Window(win)
        } else {
            log::error!("compositor has neither layer-shell nor xdg-shell");
            return;
        };
        self.surfaces.push(OutSurface::new(output, kind));
    }

    pub fn remove_output_surfaces(&mut self, output: &wl_output::WlOutput) {
        self.surfaces.retain(|s| s.output.as_ref() != Some(output));
        self.redraw_card();
    }

    fn surface_index(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.surfaces.iter().position(|s| s.wl_surface() == surface)
    }

    fn configure_surface(&mut self, surface: &wl_surface::WlSurface, w: u32, h: u32) {
        let Some(i) = self.surface_index(surface) else { return };
        let fallback = self.surfaces[i]
            .output
            .as_ref()
            .and_then(|o| self.outputs.info(o))
            .and_then(|info| info.logical_size)
            .map(|(w, h)| (w as u32, h as u32))
            .unwrap_or((1280, 720));
        let s = &mut self.surfaces[i];
        s.size = (if w == 0 { fallback.0 } else { w }, if h == 0 { fallback.1 } else { h });
        s.configured = true;
        s.dirty = true;
        // Nothing drawn yet: the card belongs on the first configured surface.
        self.flush();
    }

    // ---- input ---------------------------------------------------------------------------

    pub fn start_blink(&mut self) {
        if self.blink.is_some() {
            return;
        }
        let tok = self.handle.insert_source(Timer::from_duration(BLINK), |_, _, app: &mut App| {
            app.ui.caret = !app.ui.caret;
            app.redraw_card();
            TimeoutAction::ToDuration(BLINK)
        });
        self.blink = tok.ok();
        let tok = self.handle.insert_source(Timer::from_duration(Duration::from_secs(1)), |_, _, app: &mut App| {
            app.on_tick();
            TimeoutAction::ToDuration(Duration::from_secs(1))
        });
        self.tick = tok.ok();
    }

    pub fn stop_blink(&mut self) {
        for t in [self.blink.take(), self.tick.take(), self.anim.take()].into_iter().flatten() {
            self.handle.remove(t);
        }
        self.stop_repeat();
    }

    /// The clock changed minute, or the input view sat idle: redraw, or go back to the clock.
    fn on_tick(&mut self) {
        let now = crate::ui::clock_now();
        if now != self.ui.clock {
            self.ui.clock = now;
            self.redraw_card();
        }
        let idle = self.last_input.elapsed() >= IDLE;
        if idle && self.ui.reveal.input() && !self.ui.busy && self.ui.password().is_empty() {
            self.show_input(false);
        }
    }

    /// Switch between the clock and the input view, animated.
    pub fn show_input(&mut self, input: bool) {
        let now = Instant::now();
        if !self.ui.reveal.set(input, now) {
            return;
        }
        if !input {
            self.ui.clear();
            if self.ui.status.as_ref().is_some_and(|s| s.tone != crate::ui::Tone::Error) {
                self.ui.status = None;
            }
        }
        self.redraw_card();
        if self.anim.is_none() {
            let tok = self.handle.insert_source(Timer::from_duration(FRAME), |_, _, app: &mut App| {
                app.redraw_card();
                if app.ui.reveal.settled(Instant::now()) {
                    app.anim = None;
                    TimeoutAction::Drop
                } else {
                    TimeoutAction::ToDuration(FRAME)
                }
            });
            self.anim = tok.ok();
        }
    }

    fn stop_repeat(&mut self) {
        if let Some((_, t)) = self.repeat.take() {
            self.handle.remove(t);
        }
    }

    fn refresh_keyboard_ui(&mut self) {
        let caps = self.kb.caps_lock();
        let n = self.kb.layout_count();
        let active = self.kb.layouts.active();
        let label = layout_label(&self.cfg.layouts, n, active, &self.kb.layout_name(active));
        let avail = n > 1;
        if caps != self.ui.caps || label != self.ui.layout || avail != self.ui.layouts_available {
            self.ui.caps = caps;
            self.ui.layout = label;
            self.ui.layouts_available = avail;
            self.redraw_card();
        }
    }

    pub fn act(&mut self, action: Action) {
        self.ui.caret = true;
        self.last_input = Instant::now();
        if !self.ui.reveal.input() {
            // Clock view: Space, Enter or typing open the input view (what's typed is kept);
            // layout and power keys work as usual; the rest waits for the field.
            match &action {
                Action::Submit | Action::Text(_) if self.ui.busy => return,
                Action::Submit => return self.show_input(true),
                Action::Text(t) if t == " " => return self.show_input(true),
                Action::Text(_) => self.show_input(true),
                Action::CycleLayout | Action::F(_) => {}
                _ => return,
            }
        } else if matches!(action, Action::Escape) && !self.ui.busy {
            self.show_input(false);
        }
        match &action {
            Action::Text(t) if !self.ui.busy => {
                self.ui.push(t);
            }
            Action::Backspace if !self.ui.busy => self.ui.backspace(),
            Action::Clear if !self.ui.busy => self.ui.clear(),
            Action::CycleLayout => {
                self.kb.cycle_layout();
                self.refresh_keyboard_ui();
            }
            Action::Submit if !self.ui.busy => match self.mode {
                ModeState::Login(_) => crate::login::submit(self),
                ModeState::Lock(_) => crate::lock::submit(self),
            },
            _ => {}
        }
        match self.mode {
            ModeState::Login(_) => crate::login::key(self, &action),
            ModeState::Lock(_) => crate::lock::key(self, &action),
        }
        self.redraw_card();
    }

    fn click(&mut self, surface: &wl_surface::WlSurface, x: f64, y: f64) {
        let Some(i) = self.surface_index(surface) else { return };
        self.last_input = Instant::now();
        if !self.ui.reveal.input() {
            // Clock view: a click anywhere opens the input view.
            return self.show_input(true);
        }
        if self.ui.power_buttons {
            let (w, h) = self.surfaces[i].size;
            let hit =
                Geometry::power_buttons(w as f32, h as f32).into_iter().find(|(_, r)| r.contains(x as f32, y as f32));
            if let Some((p, _)) = hit {
                self.act(Action::F(if p == crate::logind::Power::PowerOff { 1 } else { 2 }));
                return;
            }
        }
        if self.card_index() != Some(i) {
            return;
        }
        let login = matches!(self.mode, ModeState::Login(_));
        let corner = Corner::new(login, self.surfaces[i].size.1 as f32);
        match corner.hit(x as f32, y as f32, &self.ui) {
            Some(Hit::Layout) => self.act(Action::CycleLayout),
            Some(Hit::PrevSession) => self.pick(-1),
            Some(Hit::NextSession) => self.pick(1),
            None => {}
        }
    }

    fn pick(&mut self, d: i32) {
        match self.mode {
            ModeState::Login(_) => crate::login::pick(self, d),
            ModeState::Lock(_) => crate::lock::pick(self, d),
        }
        self.redraw_card();
    }
}

// ---- Wayland plumbing ---------------------------------------------------------------------

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        factor: i32,
    ) {
        if let Some(i) = self.surface_index(surface) {
            if self.surfaces[i].scale != factor {
                self.surfaces[i].scale = factor;
                self.surfaces[i].dirty = true;
            }
        }
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        match self.mode {
            ModeState::Login(_) => crate::login::new_output(self, output),
            ModeState::Lock(_) => crate::lock::new_output(self, output),
        }
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.remove_output_surfaces(&output);
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = Some(seat.get_keyboard(qh, ()));
        }
        if cap == Capability::Pointer && self.pointer.is_none() {
            let cursor_surface = self.compositor.create_surface(qh);
            match self.seat.get_pointer_with_theme(qh, &seat, self.shm.wl_shm(), cursor_surface, ThemeSpec::System) {
                Ok(p) => self.pointer = Some(p),
                Err(e) => log::warn!("pointer: {e}"),
            }
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Keyboard {
            if let Some(k) = self.keyboard.take() {
                k.release();
            }
            self.stop_repeat();
        }
        if cap == Capability::Pointer {
            if let Some(p) = self.pointer.take() {
                p.pointer().release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for App {
    fn event(
        app: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { .. } => log::trace!("keyboard enter"),
            wl_keyboard::Event::Keymap { format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1), fd, size } => {
                log::trace!("keymap {size} bytes");
                app.kb.set_keymap_fd(fd, size);
                app.refresh_keyboard_ui();
            }
            wl_keyboard::Event::Leave { .. } => app.stop_repeat(),
            wl_keyboard::Event::Modifiers { mods_depressed, mods_latched, mods_locked, group, .. } => {
                app.kb.set_modifiers(mods_depressed, mods_latched, mods_locked, group);
                app.refresh_keyboard_ui();
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                app.kb.repeat = (rate > 0).then_some((rate as u32, delay.max(0) as u32));
            }
            wl_keyboard::Event::Key { key, state, .. } => {
                let pressed = matches!(state, WEnum::Value(wl_keyboard::KeyState::Pressed));
                if !pressed {
                    if app.repeat.as_ref().is_some_and(|(k, _)| *k == key) {
                        app.stop_repeat();
                    }
                    return;
                }
                app.stop_repeat();
                let Some(action) = app.kb.press(key) else { return };
                let repeats = matches!(action, Action::Text(_) | Action::Backspace) && app.kb.key_repeats(key);
                app.act(action.clone());
                if let (true, Some((rate, delay))) = (repeats, app.kb.repeat) {
                    let every = Duration::from_micros(1_000_000 / rate.max(1) as u64);
                    let tok = app.handle.insert_source(
                        Timer::from_duration(Duration::from_millis(delay as u64)),
                        move |_, _, app: &mut App| {
                            app.act(action.clone());
                            TimeoutAction::ToDuration(every)
                        },
                    );
                    if let Ok(t) = tok {
                        app.repeat = Some((key, t));
                    }
                }
            }
            _ => {}
        }
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        conn: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for ev in events {
            log::trace!("pointer {:?} on {:?} at {:?}", ev.kind, ev.surface.id(), ev.position);
            match ev.kind {
                PointerEventKind::Enter { .. } => {
                    if let Some(p) = &self.pointer {
                        let _ = p.set_cursor(conn, CursorIcon::Default);
                    }
                    self.pointer_on(&ev.surface);
                }
                PointerEventKind::Motion { .. } => self.pointer_on(&ev.surface),
                PointerEventKind::Press { button: BTN_LEFT, .. } => {
                    self.click(&ev.surface, ev.position.0, ev.position.1)
                }
                _ => {}
            }
        }
    }
}

impl App {
    /// The card follows the pointer to its output.
    fn pointer_on(&mut self, surface: &wl_surface::WlSurface) {
        if self.surface_index(surface).is_none() {
            return;
        }
        // flush() notices the move and repaints both outputs.
        self.card_surface = Some(surface.id());
    }
}

impl Dispatch<wl_buffer::WlBuffer, Arc<AtomicBool>> for App {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        busy: &Arc<AtomicBool>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            busy.store(false, Ordering::Release);
        }
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        self.surfaces.retain(|s| s.wl_surface() != layer.wl_surface());
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        c: LayerSurfaceConfigure,
        _: u32,
    ) {
        self.configure_surface(&layer.wl_surface().clone(), c.new_size.0, c.new_size.1);
    }
}

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        // A login screen doesn't close.
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, win: &Window, c: WindowConfigure, _: u32) {
        let w = c.new_size.0.map_or(0, |v| v.get());
        let h = c.new_size.1.map_or(0, |v| v.get());
        self.configure_surface(&win.wl_surface().clone(), w, h);
    }
}

impl SessionLockHandler for App {
    fn locked(&mut self, _: &Connection, _: &QueueHandle<Self>, lock: SessionLock) {
        crate::lock::locked(self, lock);
    }
    fn finished(&mut self, _: &Connection, _: &QueueHandle<Self>, lock: SessionLock) {
        crate::lock::finished(self, lock);
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: SessionLockSurface,
        c: SessionLockSurfaceConfigure,
        _: u32,
    ) {
        self.configure_surface(&surface.wl_surface().clone(), c.new_size.0, c.new_size.1);
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_seat!(App);
delegate_pointer!(App);
delegate_layer!(App);
delegate_xdg_shell!(App);
delegate_xdg_window!(App);
delegate_session_lock!(App);
delegate_registry!(App);

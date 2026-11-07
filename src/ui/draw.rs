//! Pixels: wallpaper scaled to fill, the frosted card behind the form, the form itself, and the
//! copy into wl_shm buffers (XRGB8888, i.e. B G R X bytes).

use std::path::Path;

use image::RgbaImage;
use tiny_skia::{FillRule, Mask, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform};

use super::text::{Align, Painter, Style};
use super::{smooth, Corner, Geometry, Rect, Tone, Ui};
use crate::logind::Power;

const WHITE: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
const SOFT: [f32; 4] = [1.0, 1.0, 1.0, 0.86];
const FAINT: [f32; 4] = [1.0, 1.0, 1.0, 0.5];

pub struct Wallpaper(Option<RgbaImage>);

impl Wallpaper {
    /// Never fails: a missing or broken wallpaper falls back to a plain dark background.
    pub fn load(path: Option<&Path>) -> Self {
        let Some(path) = path else { return Self(None) };
        match image::open(path) {
            Ok(img) => Self(Some(img.into_rgba8())),
            Err(e) => {
                log::warn!("wallpaper {}: {e}", path.display());
                Self(None)
            }
        }
    }

    /// The image scaled to cover `w`×`h` and center-cropped, as an opaque pixmap.
    fn fill(&self, w: u32, h: u32) -> Pixmap {
        let mut pm = Pixmap::new(w, h).expect("output size");
        let Some(img) = &self.0 else {
            // Plain vertical gradient, dark slate.
            let data = pm.data_mut();
            for y in 0..h {
                let t = y as f32 / h.max(1) as f32;
                let c = [(28.0 - 10.0 * t) as u8, (32.0 - 10.0 * t) as u8, (40.0 - 12.0 * t) as u8, 255];
                for x in 0..w {
                    let i = ((y * w + x) * 4) as usize;
                    data[i..i + 4].copy_from_slice(&c);
                }
            }
            return pm;
        };
        let (iw, ih) = img.dimensions();
        let s = (w as f32 / iw as f32).max(h as f32 / ih as f32);
        let cw = ((w as f32 / s).round() as u32).clamp(1, iw);
        let ch = ((h as f32 / s).round() as u32).clamp(1, ih);
        let (cx, cy) = ((iw - cw) / 2, (ih - ch) / 2);
        let scaled = if (cw, ch, cx, cy) == (w, h, 0, 0) {
            img.clone()
        } else {
            let crop = image::imageops::crop_imm(img, cx, cy, cw, ch).to_image();
            image::imageops::resize(&crop, w, h, image::imageops::FilterType::Triangle)
        };
        // Premultiply onto black: the result is opaque.
        for (d, s) in pm.data_mut().chunks_exact_mut(4).zip(scaled.as_raw().chunks_exact(4)) {
            let a = s[3] as u32;
            d[0] = (s[0] as u32 * a / 255) as u8;
            d[1] = (s[1] as u32 * a / 255) as u8;
            d[2] = (s[2] as u32 * a / 255) as u8;
            d[3] = 255;
        }
        pm
    }
}

/// Integer rect in buffer px.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Everything about one output that only changes with its size: background bytes and the card's
/// frosted backdrop.
pub struct Scene {
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub bg_xrgb: Vec<u8>,
    bg: Pixmap,
    pub card: IRect,
    card_bg: Pixmap,
    /// The bottom-left controls (session picker, layout), in logical and buffer px.
    pub corner: Corner,
    pub corner_px: IRect,
    /// The bottom-right power buttons (login screen), in buffer px.
    pub power_px: Option<IRect>,
}

pub struct Look {
    pub dim: f32,
    pub blur: f32,
}

impl Scene {
    #[allow(clippy::too_many_arguments)]
    pub fn build(wall: &Wallpaper, w: u32, h: u32, scale: f32, geom: &Geometry, look: &Look, login: bool) -> Self {
        let bg = wall.fill(w, h);
        let card = card_rect(geom, w, h, scale);
        let card_bg = frosted(&bg, card, look, geom, scale);
        let corner = Corner::new(login, h as f32 / scale);
        let corner_px = to_px(corner.area, w, h, scale);
        let power_px = login.then(|| to_px(Corner::power_area(w as f32 / scale, h as f32 / scale), w, h, scale));
        Self { width: w, height: h, scale, bg_xrgb: to_xrgb(&bg), bg, card, card_bg, corner, corner_px, power_px }
    }

    /// The card changed size: place it again and redo its backdrop. The background stays.
    pub fn relayout(&mut self, geom: &Geometry, look: &Look) {
        self.card = card_rect(geom, self.width, self.height, self.scale);
        self.card_bg = frosted(&self.bg, self.card, look, geom, self.scale);
    }

    /// The wallpaper under the card, for outputs that don't show it.
    pub fn plain_card(&self) -> Pixmap {
        crop(&self.bg, self.card)
    }

    /// The bottom-left corner: bare wallpaper, or with the controls `drop` px below their place
    /// on the output showing the card.
    pub fn render_corner(&self, ui: Option<&Ui>, drop: f32, painter: &mut Painter) -> Pixmap {
        let mut pm = crop(&self.bg, self.corner_px);
        if let Some(ui) = ui {
            let (ox, oy) = (self.corner_px.x as f32 / self.scale, self.corner_px.y as f32 / self.scale - drop);
            draw_corner(&mut pm, &self.corner, ox, oy, ui, painter, self.scale);
        }
        pm
    }

    /// The power buttons' corner, like `render_corner`.
    pub fn render_power(&self, show: bool, drop: f32) -> Option<(IRect, Pixmap)> {
        let r = self.power_px?;
        let mut pm = crop(&self.bg, r);
        if show {
            let (ox, oy) = (r.x as f32 / self.scale, r.y as f32 / self.scale - drop);
            let t = Transform::from_scale(self.scale, self.scale).pre_translate(-ox, -oy);
            let (w, h) = (self.width as f32 / self.scale, self.height as f32 / self.scale);
            for (p, b) in Geometry::power_buttons(w, h) {
                power_button(&mut pm, p, b, t);
            }
        }
        Some((r, pm))
    }

    /// The card at point `reveal` of the transition: the clock fades out drifting up, the name
    /// and field fade in rising into place. Messages stay put.
    pub fn render_card(&self, geom: &Geometry, ui: &Ui, reveal: f32, painter: &mut Painter) -> Pixmap {
        let mut pm = self.card_bg.clone();
        let s = self.scale;
        let form = smooth((reveal - 0.1) / 0.75);
        let clock = 1.0 - smooth(reveal / 0.6);
        let layer = |pm: &mut Pixmap, alpha: f32, dy: f32, draw: &mut dyn FnMut(&mut Pixmap)| {
            if alpha <= 0.0 {
                return;
            }
            if alpha >= 1.0 && dy == 0.0 {
                return draw(pm);
            }
            let Some(mut l) = Pixmap::new(pm.width(), pm.height()) else { return };
            draw(&mut l);
            let paint = PixmapPaint { opacity: alpha, ..Default::default() };
            pm.draw_pixmap(0, (dy * s).round() as i32, l.as_ref(), &paint, Transform::identity(), None);
        };
        layer(&mut pm, clock, -14.0 * (1.0 - clock), &mut |l| draw_clock(l, geom, ui, painter, s));
        layer(&mut pm, form, 14.0 * (1.0 - form), &mut |l| draw_form(l, geom, ui, painter, s));
        draw_status(&mut pm, geom, ui, painter, s);
        pm
    }
}

/// A logical rect in buffer px, inside a `w`×`h` buffer.
fn to_px(r: Rect, w: u32, h: u32, scale: f32) -> IRect {
    let px = |v: f32| (v * scale).round().max(0.0) as u32;
    let (x, y) = (px(r.x).min(w - 1), px(r.y).min(h - 1));
    IRect { x, y, w: px(r.w).clamp(1, w - x), h: px(r.h).clamp(1, h - y) }
}

/// The card's rect in buffer px on a `w`×`h` buffer.
fn card_rect(geom: &Geometry, w: u32, h: u32, scale: f32) -> IRect {
    let (ox, oy) = geom.origin(w as f32 / scale, h as f32 / scale);
    let cw = ((geom.card.w * scale).round() as u32).clamp(1, w);
    let ch = ((geom.card.h * scale).round() as u32).clamp(1, h);
    IRect { x: ((ox * scale).round() as u32).min(w - cw), y: ((oy * scale).round() as u32).min(h - ch), w: cw, h: ch }
}

/// A round glassy button with a shut-down or reboot icon, drawn into the background.
fn power_button(pm: &mut Pixmap, p: Power, r: Rect, t: Transform) {
    let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
    if let Some(c) = PathBuilder::from_circle(cx, cy, r.w / 2.0) {
        pm.fill_path(&c, &paint([0.0, 0.0, 0.0, 0.28]), FillRule::Winding, t, None);
    }
    if let Some(c) = PathBuilder::from_circle(cx, cy, r.w / 2.0 - 0.5) {
        pm.stroke_path(&c, &paint([1.0, 1.0, 1.0, 0.18]), &Stroke { width: 1.0, ..Default::default() }, t, None);
    }
    let rad = 8.0;
    let color = [1.0, 1.0, 1.0, 0.9];
    let mut pb = PathBuilder::new();
    match p {
        Power::PowerOff => {
            // An arc open at the top, and the bar through the gap.
            arc(&mut pb, cx, cy, rad, -60.0, 300.0);
            stroke(pm, pb.finish(), color, 2.0, t);
            let mut bar = PathBuilder::new();
            bar.move_to(cx, cy - rad - 2.0);
            bar.line_to(cx, cy - 1.0);
            stroke(pm, bar.finish(), color, 2.0, t);
        }
        Power::Reboot => {
            // An almost closed circle, with an arrow head where it ends.
            arc(&mut pb, cx, cy, rad, -40.0, 280.0);
            stroke(pm, pb.finish(), color, 2.0, t);
            // Arrow head at the arc's start, pointing back along it (anticlockwise).
            let a = (-40.0f32).to_radians();
            let (px, py) = (cx + rad * a.cos(), cy + rad * a.sin());
            let (dx, dy) = (a.sin(), -a.cos());
            let (nx, ny) = (a.cos(), a.sin());
            let mut head = PathBuilder::new();
            head.move_to(px + dx * 4.0, py + dy * 4.0);
            head.line_to(px - dx * 2.0 + nx * 4.0, py - dy * 2.0 + ny * 4.0);
            head.line_to(px - dx * 2.0 - nx * 4.0, py - dy * 2.0 - ny * 4.0);
            head.close();
            fill(pm, head.finish(), color, t);
        }
    }
}

/// Clockwise arc from `start` degrees (0 = right, 90 = down) through `sweep` degrees, as lines.
fn arc(pb: &mut PathBuilder, cx: f32, cy: f32, r: f32, start: f32, sweep: f32) {
    let steps = (sweep.abs() / 6.0).ceil() as usize;
    for i in 0..=steps {
        let a = (start + sweep * i as f32 / steps as f32).to_radians();
        let (x, y) = (cx + r * a.cos(), cy + r * a.sin());
        if i == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
}

fn crop(src: &Pixmap, r: IRect) -> Pixmap {
    let mut out = Pixmap::new(r.w, r.h).expect("crop size");
    let (sw, ow) = (src.width() as usize * 4, r.w as usize * 4);
    for row in 0..r.h as usize {
        let s = (r.y as usize + row) * sw + r.x as usize * 4;
        out.data_mut()[row * ow..row * ow + ow].copy_from_slice(&src.data()[s..s + ow]);
    }
    out
}

/// Card backdrop: the wallpaper under it, blurred and darkened inside the card's rounded rect.
fn frosted(bg: &Pixmap, card: IRect, look: &Look, geom: &Geometry, scale: f32) -> Pixmap {
    let mut plain = crop(bg, card);
    if look.dim <= 0.0 && look.blur <= 0.0 {
        return plain;
    }
    let mut glass = if look.blur > 0.0 {
        // Blur a margin around the card too so its edges sample real neighbours.
        let r = (look.blur * scale).round().max(1.0) as u32;
        let m = 3 * r;
        let x0 = card.x.saturating_sub(m);
        let y0 = card.y.saturating_sub(m);
        let x1 = (card.x + card.w + m).min(bg.width());
        let y1 = (card.y + card.h + m).min(bg.height());
        let mut big = crop(bg, IRect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 });
        let (bw, bh) = (big.width() as usize, big.height() as usize);
        for _ in 0..3 {
            box_blur(big.data_mut(), bw, bh, r as usize);
        }
        crop(&big, IRect { x: card.x - x0, y: card.y - y0, w: card.w, h: card.h })
    } else {
        plain.clone()
    };
    if look.dim > 0.0 {
        let k = 1.0 - look.dim;
        for px in glass.data_mut().chunks_exact_mut(4) {
            for c in &mut px[..3] {
                *c = (*c as f32 * k) as u8;
            }
        }
    }
    let rect = Rect { x: 0.0, y: 0.0, w: geom.card.w, h: geom.card.h };
    let t = Transform::from_scale(scale, scale);
    if let (Some(mut mask), Some(path)) = (Mask::new(card.w, card.h), rrect(rect, 20.0)) {
        mask.fill_path(&path, FillRule::Winding, true, t);
        plain.draw_pixmap(0, 0, glass.as_ref(), &PixmapPaint::default(), Transform::identity(), Some(&mask));
        stroke(&mut plain, rrect(inset(rect, 0.5), 19.5), [1.0, 1.0, 1.0, 0.12], 1.0, t);
    }
    plain
}

/// One horizontal + one vertical box-blur pass over opaque RGBA.
fn box_blur(data: &mut [u8], w: usize, h: usize, r: usize) {
    let mut line = vec![0u8; w.max(h) * 4];
    let pass = |data: &mut [u8], line: &mut [u8], n: usize, len: usize, at: &dyn Fn(usize, usize) -> usize| {
        let div = (2 * r + 1) as u32;
        for l in 0..n {
            for k in 0..len {
                line[k * 4..k * 4 + 4].copy_from_slice(&data[at(l, k)..at(l, k) + 4]);
            }
            let px = |k: isize| -> usize { k.clamp(0, len as isize - 1) as usize * 4 };
            let mut acc = [0u32; 3];
            for k in -(r as isize)..=(r as isize) {
                for c in 0..3 {
                    acc[c] += line[px(k) + c] as u32;
                }
            }
            for k in 0..len {
                let o = at(l, k);
                for c in 0..3 {
                    data[o + c] = (acc[c] / div) as u8;
                    acc[c] += line[px(k as isize + r as isize + 1) + c] as u32;
                    acc[c] -= line[px(k as isize - r as isize) + c] as u32;
                }
            }
        }
    };
    pass(data, &mut line, h, w, &|l, k| (l * w + k) * 4);
    pass(data, &mut line, w, h, &|l, k| (k * w + l) * 4);
}

fn draw_form(pm: &mut Pixmap, g: &Geometry, ui: &Ui, p: &mut Painter, s: f32) {
    let t = Transform::from_scale(s, s);

    let name = Style { bold: true, ..Style::new(super::NAME_SIZE * s, WHITE) };
    p.draw(pm, &ui.name, &name, g.name.x * s, (g.name.y + 1.0) * s, g.name.w * s);

    // Field.
    let f = g.field;
    fill(pm, rrect(f, 12.0), [1.0, 1.0, 1.0, 0.10], t);
    let border = if ui.busy { 0.22 } else { 0.55 };
    stroke(pm, rrect(inset(f, 0.75), 11.25), [1.0, 1.0, 1.0, border], 1.5, t);

    let text_x = f.x + 16.0;
    let text_end = f.x + f.w - 16.0;
    let mid = f.y + f.h / 2.0;
    let mut caret_x = text_x;
    let content = Style { align: Align::Left, ..Style::new(15.0 * s, WHITE) };
    if ui.password().is_empty() {
        if !ui.busy {
            let ph = Style { color: FAINT, ..content };
            p.draw(pm, &ui.placeholder, &ph, (text_x + 3.0) * s, (mid - 10.0) * s, (text_end - text_x) * s);
        }
    } else if ui.masked {
        let step = 15.0;
        let room = ((text_end - text_x - 6.0) / step).floor().max(1.0) as usize;
        let n = ui.password_len().min(room);
        let alpha = if ui.busy { 0.45 } else { 1.0 };
        for i in 0..n {
            let cx = text_x + step / 2.0 + i as f32 * step;
            if let Some(c) = PathBuilder::from_circle(cx, mid, 4.5) {
                pm.fill_path(&c, &paint([1.0, 1.0, 1.0, alpha]), FillRule::Winding, t, None);
            }
        }
        caret_x = text_x + n as f32 * step + 2.0;
    } else {
        let w = p.measure(ui.password(), &content) / s;
        p.draw(pm, ui.password(), &content, text_x * s, (mid - 10.0) * s, (text_end - text_x) * s);
        caret_x = (text_x + w + 2.0).min(text_end);
    }
    if ui.caret && !ui.busy {
        fill(pm, rect_path(Rect { x: caret_x, y: mid - 11.0, w: 2.0, h: 22.0 }), WHITE, t);
    }

    if ui.caps && g.caps.h > 0.0 {
        let st = Style::new(super::SMALL * s, SOFT);
        p.draw(pm, "Caps Lock is on", &st, g.caps.x * s, g.caps.y * s, g.caps.w * s);
    }
}

/// The status message, under the field or the clock.
fn draw_status(pm: &mut Pixmap, g: &Geometry, ui: &Ui, p: &mut Painter, s: f32) {
    if let Some(status) = &ui.status {
        let color = match status.tone {
            Tone::Error => WHITE,
            Tone::Info => SOFT,
            Tone::Busy => FAINT,
        };
        let st = Style { max_lines: g.status_lines.max(1), ..Style::new(super::SMALL * s, color) };
        p.draw(pm, &status.text, &st, g.status.x * s, g.status.y * s, g.status.w * s);
    }
}

/// The clock view: the time, large, and the date under it.
fn draw_clock(pm: &mut Pixmap, g: &Geometry, ui: &Ui, p: &mut Painter, s: f32) {
    let time = Style { bold: true, ..Style::new(super::TIME_SIZE * s, WHITE) };
    p.draw(pm, &ui.clock.0, &time, g.time.x * s, g.time.y * s, g.time.w * s);
    let date = Style::new(15.0 * s, SOFT);
    p.draw(pm, &ui.clock.1, &date, g.date.x * s, g.date.y * s, g.date.w * s);
}

/// Session picker and layout pill, as glassy pills like the power buttons. (ox, oy): the
/// pixmap's origin in logical output px.
fn draw_corner(pm: &mut Pixmap, c: &Corner, ox: f32, oy: f32, ui: &Ui, p: &mut Painter, s: f32) {
    let t = Transform::from_scale(s, s).pre_translate(-ox, -oy);
    let at = |r: Rect| Rect { x: r.x - ox, y: r.y - oy, ..r };
    let glass = |pm: &mut Pixmap, r: Rect| {
        fill(pm, rrect(r, r.h / 2.0), [0.0, 0.0, 0.0, 0.28], t);
        stroke(pm, rrect(inset(r, 0.5), r.h / 2.0 - 0.5), [1.0, 1.0, 1.0, 0.18], 1.0, t);
    };
    if let (Some(r), Some((name, arrows))) = (c.session, &ui.session) {
        glass(pm, r);
        let st = Style::new(14.0 * s, SOFT);
        let r0 = at(r);
        let inset_x = if *arrows { c.prev.w } else { 14.0 };
        p.draw(pm, name, &st, (r0.x + inset_x) * s, (r0.y + 11.0) * s, (r0.w - 2.0 * inset_x) * s);
        if *arrows {
            chevron(pm, c.prev, -1.0, t);
            chevron(pm, c.next, 1.0, t);
        }
    }
    if ui.layouts_available || !ui.layout.is_empty() {
        glass(pm, c.layout);
        let st = Style { bold: true, ..Style::new(13.0 * s, if ui.layouts_available { SOFT } else { FAINT }) };
        let l = at(c.layout);
        p.draw(pm, &ui.layout, &st, l.x * s, (l.y + 11.5) * s, l.w * s);
    }
}

fn chevron(pm: &mut Pixmap, r: Rect, dir: f32, t: Transform) {
    let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
    let mut pb = PathBuilder::new();
    pb.move_to(cx - 3.0 * dir, cy - 6.0);
    pb.line_to(cx + 3.0 * dir, cy);
    pb.line_to(cx - 3.0 * dir, cy + 6.0);
    stroke(pm, pb.finish(), [1.0, 1.0, 1.0, 0.75], 2.0, t);
}

fn paint(c: [f32; 4]) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8((c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8, (c[3] * 255.0) as u8);
    p.anti_alias = true;
    p
}

fn fill(pm: &mut Pixmap, path: Option<tiny_skia::Path>, c: [f32; 4], t: Transform) {
    if let Some(path) = path {
        pm.fill_path(&path, &paint(c), FillRule::Winding, t, None);
    }
}

fn stroke(pm: &mut Pixmap, path: Option<tiny_skia::Path>, c: [f32; 4], width: f32, t: Transform) {
    if let Some(path) = path {
        let st = Stroke {
            width,
            line_cap: tiny_skia::LineCap::Round,
            line_join: tiny_skia::LineJoin::Round,
            ..Default::default()
        };
        pm.stroke_path(&path, &paint(c), &st, t, None);
    }
}

fn inset(r: Rect, d: f32) -> Rect {
    Rect { x: r.x + d, y: r.y + d, w: (r.w - 2.0 * d).max(0.0), h: (r.h - 2.0 * d).max(0.0) }
}

fn rect_path(r: Rect) -> Option<tiny_skia::Path> {
    tiny_skia::Rect::from_xywh(r.x, r.y, r.w, r.h).map(PathBuilder::from_rect)
}

fn rrect(r: Rect, rad: f32) -> Option<tiny_skia::Path> {
    let rad = rad.max(0.0).min(r.w / 2.0).min(r.h / 2.0);
    let (x, y, w, h) = (r.x, r.y, r.w, r.h);
    let k = 0.552_284_8 * rad;
    let mut pb = PathBuilder::new();
    pb.move_to(x + rad, y);
    pb.line_to(x + w - rad, y);
    pb.cubic_to(x + w - rad + k, y, x + w, y + rad - k, x + w, y + rad);
    pb.line_to(x + w, y + h - rad);
    pb.cubic_to(x + w, y + h - rad + k, x + w - rad + k, y + h, x + w - rad, y + h);
    pb.line_to(x + rad, y + h);
    pb.cubic_to(x + rad - k, y + h, x, y + h - rad + k, x, y + h - rad);
    pb.line_to(x, y + rad);
    pb.cubic_to(x, y + rad - k, x + rad - k, y, x + rad, y);
    pb.close();
    pb.finish()
}

/// RGBA (opaque, so premultiplied = straight) to wl_shm XRGB8888 bytes.
fn to_xrgb(pm: &Pixmap) -> Vec<u8> {
    let mut out = vec![0u8; pm.data().len()];
    for (d, s) in out.chunks_exact_mut(4).zip(pm.data().chunks_exact(4)) {
        d.copy_from_slice(&[s[2], s[1], s[0], 255]);
    }
    out
}

/// Copy `pm` into an XRGB8888 canvas of `stride` bytes per row at (x, y).
pub fn blit(canvas: &mut [u8], stride: usize, pm: &Pixmap, x: u32, y: u32) {
    let w = pm.width() as usize;
    for row in 0..pm.height() as usize {
        let src = &pm.data()[row * w * 4..(row + 1) * w * 4];
        let o = (y as usize + row) * stride + x as usize * 4;
        let Some(dst) = canvas.get_mut(o..o + w * 4) else { return };
        for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
            d.copy_from_slice(&[s[2], s[1], s[0], 255]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blur_keeps_flat_color_flat() {
        let (w, h) = (20, 10);
        let mut d = vec![0u8; w * h * 4];
        for px in d.chunks_exact_mut(4) {
            px.copy_from_slice(&[100, 50, 25, 255]);
        }
        box_blur(&mut d, w, h, 3);
        assert!(d.chunks_exact(4).all(|p| p == [100, 50, 25, 255]));
    }

    #[test]
    fn fill_covers_and_crops() {
        let mut img = RgbaImage::new(400, 100);
        for (x, _, p) in img.enumerate_pixels_mut() {
            *p = if x < 200 { image::Rgba([255, 0, 0, 255]) } else { image::Rgba([0, 0, 255, 255]) };
        }
        // Square output: the middle 100x100 of the image, half red half blue.
        let pm = Wallpaper(Some(img)).fill(50, 50);
        let px = |x: u32, y: u32| &pm.data()[((y * 50 + x) * 4) as usize..][..4];
        assert_eq!(px(2, 25), [255, 0, 0, 255]);
        assert_eq!(px(47, 25), [0, 0, 255, 255]);
        let plain = Wallpaper(None).fill(4, 4);
        assert!(plain.data().chunks_exact(4).all(|p| p[3] == 255));
    }

    #[test]
    fn blit_swaps_to_bgr() {
        let mut pm = Pixmap::new(1, 1).unwrap();
        pm.data_mut().copy_from_slice(&[10, 20, 30, 255]);
        let mut canvas = vec![0u8; 3 * 2 * 4];
        blit(&mut canvas, 12, &pm, 1, 1);
        assert_eq!(&canvas[16..20], &[30, 20, 10, 255]);
    }
}

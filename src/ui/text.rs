//! Text with cosmic-text, blended straight into tiny-skia pixmaps (physical px).

use cosmic_text::{Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache, Weight, Wrap};
use tiny_skia::Pixmap;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Align {
    Left,
    Center,
}

#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub size: f32,
    pub bold: bool,
    pub color: [f32; 4],
    pub align: Align,
    pub max_lines: usize,
}

impl Style {
    pub fn new(size: f32, color: [f32; 4]) -> Self {
        Self { size, bold: false, color, align: Align::Center, max_lines: 1 }
    }
}

pub struct Painter {
    fonts: FontSystem,
    swash: SwashCache,
    family: String,
}

impl Painter {
    pub fn new(family: &str) -> Self {
        let fonts = FontSystem::new();
        let known = fonts.db().faces().any(|f| f.families.iter().any(|(n, _)| n.eq_ignore_ascii_case(family)));
        if !known {
            log::warn!("font {family:?} not found, using the default sans-serif");
        }
        Self { fonts, swash: SwashCache::new(), family: if known { family.to_string() } else { String::new() } }
    }

    fn layout(&mut self, text: &str, st: &Style, width: Option<f32>) -> Buffer {
        let line_h = (st.size * 1.3).ceil();
        let fs = &mut self.fonts;
        let mut buf = Buffer::new(fs, Metrics::new(st.size, line_h));
        buf.set_wrap(if st.max_lines > 1 { Wrap::WordOrGlyph } else { Wrap::None });
        buf.set_size(width, None);
        let family = if self.family.is_empty() { Family::SansSerif } else { Family::Name(&self.family) };
        let attrs = Attrs::new().family(family).weight(if st.bold { Weight::SEMIBOLD } else { Weight::NORMAL });
        buf.set_text(text, &attrs, Shaping::Advanced, None);
        buf.shape_until_scroll(fs, false);
        buf
    }

    /// Width of a single line of text.
    pub fn measure(&mut self, text: &str, st: &Style) -> f32 {
        let buf = self.layout(text, st, None);
        buf.layout_runs().map(|r| r.line_w).fold(0.0, f32::max)
    }

    /// Draw `text` in the box starting at (x, y) of width `w`. Single-line text that doesn't fit
    /// is ellipsized; multi-line text wraps and stops at `max_lines`. Returns the height used.
    pub fn draw(&mut self, pm: &mut Pixmap, text: &str, st: &Style, x: f32, y: f32, w: f32) -> f32 {
        if text.is_empty() || w <= 1.0 {
            return 0.0;
        }
        let single: String;
        let mut text = text;
        if st.max_lines <= 1 {
            single = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
            text = &single;
            if self.measure(text, st) > w {
                let ell = self.ellipsize(text, st, w);
                return self.draw_buffer(pm, &ell, st, x, y, w);
            }
        }
        self.draw_buffer(pm, text, st, x, y, w)
    }

    fn ellipsize(&mut self, text: &str, st: &Style, w: f32) -> String {
        let chars: Vec<char> = text.chars().collect();
        let (mut lo, mut hi) = (0usize, chars.len());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            let cand: String = chars[..mid].iter().collect::<String>().trim_end().to_string() + "…";
            if self.measure(&cand, st) <= w {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        chars[..lo].iter().collect::<String>().trim_end().to_string() + "…"
    }

    fn draw_buffer(&mut self, pm: &mut Pixmap, text: &str, st: &Style, x: f32, y: f32, w: f32) -> f32 {
        let buf = self.layout(text, st, if st.max_lines > 1 { Some(w) } else { None });
        let c = st.color;
        let base = Color::rgba((c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8, (c[3] * 255.0) as u8);
        let (pw, ph) = (pm.width() as i32, pm.height() as i32);
        let data = pm.data_mut();
        let mut used = 0.0f32;
        for (i, run) in buf.layout_runs().enumerate() {
            if i >= st.max_lines.max(1) {
                break;
            }
            let dx = match st.align {
                Align::Left => 0.0,
                Align::Center => ((w - run.line_w) / 2.0).max(0.0),
            };
            used = run.line_top + run.line_height;
            for g in run.glyphs {
                let phys = g.physical((x + dx, y + run.line_y), 1.0);
                let color = g.color_opt.unwrap_or(base);
                self.swash.with_pixels(&mut self.fonts, phys.cache_key, color, |gx, gy, col| {
                    let (px, py) = (phys.x + gx, phys.y + gy);
                    if px < 0 || py < 0 || px >= pw || py >= ph {
                        return;
                    }
                    blend(data, ((py * pw + px) * 4) as usize, col);
                });
            }
        }
        used
    }
}

/// Source-over of a straight-alpha color onto premultiplied RGBA.
fn blend(data: &mut [u8], i: usize, col: Color) {
    let a = col.a() as u32;
    if a == 0 {
        return;
    }
    let inv = 255 - a;
    let px = &mut data[i..i + 4];
    px[0] = ((col.r() as u32 * a + px[0] as u32 * inv) / 255) as u8;
    px[1] = ((col.g() as u32 * a + px[1] as u32 * inv) / 255) as u8;
    px[2] = ((col.b() as u32 * a + px[2] as u32 * inv) / 255) as u8;
    px[3] = (a + px[3] as u32 * inv / 255) as u8;
}

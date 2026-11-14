//! Clock text rasterized on the CPU once per minute into a two-channel mask
//! (R = time, G = date), drawn by the GPU as a smoothly moving textured quad.

use fontdue::{Font, FontSettings};

pub const TIME_PX: f32 = 220.0;
pub const DATE_PX: f32 = 34.0;
const GAP: f32 = 8.0;
const PAD: u32 = 2; // transparent border so bilinear sampling fades out cleanly

pub struct Mask {
    pub w: u32,
    pub h: u32,
    pub rg: Vec<u8>,
}

/// The user's `sans-serif` (Light if available), resolved through fontconfig.
pub fn load_font() -> Option<Font> {
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}", "sans-serif:light"]).output().ok()?;
    let path = String::from_utf8(out.stdout).ok()?;
    let data = std::fs::read(path.trim()).ok()?;
    Font::from_bytes(data, FontSettings::default()).ok()
}

fn line_width(font: &Font, s: &str, px: f32) -> f32 {
    s.chars().map(|c| font.metrics(c, px).advance_width).sum()
}

pub fn render(font: &Font, time: &str, date: &str) -> Mask {
    let lt = font.horizontal_line_metrics(TIME_PX).expect("horizontal font");
    let ld = font.horizontal_line_metrics(DATE_PX).expect("horizontal font");
    let (wt, wd) = (line_width(font, time, TIME_PX), line_width(font, date, DATE_PX));
    let w = wt.max(wd).ceil() as u32 + PAD * 2;
    let h = (lt.ascent - lt.descent + GAP + ld.ascent - ld.descent).ceil() as u32 + PAD * 2;
    let mut rg = vec![0u8; (w * h * 2) as usize];

    let mut draw = |s: &str, px: f32, line_w: f32, baseline: f32, channel: usize| {
        let mut pen = (w as f32 - line_w) / 2.0;
        for c in s.chars() {
            let (m, bmp) = font.rasterize(c, px);
            let x0 = (pen + m.xmin as f32).round() as i32;
            let y0 = (baseline - m.height as f32 - m.ymin as f32).round() as i32;
            for gy in 0..m.height {
                for gx in 0..m.width {
                    let (x, y) = (x0 + gx as i32, y0 + gy as i32);
                    if x >= 0 && y >= 0 && (x as u32) < w && (y as u32) < h {
                        let i = ((y as u32 * w + x as u32) * 2) as usize + channel;
                        rg[i] = rg[i].max(bmp[gy * m.width + gx]);
                    }
                }
            }
            pen += m.advance_width;
        }
    };
    let base_t = PAD as f32 + lt.ascent;
    let base_d = base_t - lt.descent + GAP + ld.ascent;
    draw(time, TIME_PX, wt, base_t, 0);
    draw(date, DATE_PX, wd, base_d, 1);
    Mask { w, h, rg }
}

#[cfg(test)]
mod tests {
    #[test]
    fn mask_has_ink_in_both_channels() {
        let Some(font) = super::load_font() else { return }; // no fontconfig in this environment
        let m = super::render(&font, "21:54", "Saturday, 14 November");
        assert_eq!(m.rg.len(), (m.w * m.h * 2) as usize);
        assert!(m.rg.iter().step_by(2).any(|&v| v > 200), "time channel empty");
        assert!(m.rg.iter().skip(1).step_by(2).any(|&v| v > 200), "date channel empty");
    }
}

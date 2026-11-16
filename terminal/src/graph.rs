//! Area history in terminal pixels. Samples stay optional: missing is not zero.
use crate::theme::Theme;
use ratatui::{buffer::Buffer, layout::Rect, style::Color, widgets::Widget};

pub struct Chart {
    pub primary: Vec<(f64, f64)>,
    pub secondary: Vec<(f64, f64)>,
    pub start: f64,
    pub end: f64,
    pub upper: f64,
    pub color: Color,
}
impl Chart {
    pub fn raster(&self, size: (u32, u32), theme: Theme) -> image::DynamicImage {
        use crate::images::{mix, rgba};
        let (w, h) = size;
        let bg = rgba(theme.bg);
        let mut image = image::RgbaImage::from_pixel(w, h, bg);
        for y in 0..h {
            for x in 0..w {
                if (y == h / 4 || y == h / 2 || y == h * 3 / 4 || y == h - 1) && x % 8 < 3 {
                    image.put_pixel(x, y, mix(bg, rgba(theme.edge), 0.65));
                }
            }
        }
        for (data, color, opacity) in [
            (&self.secondary, theme.secondary, 0.20),
            (&self.primary, self.color, 0.48),
        ] {
            let samples = columns(data, self.start, self.end, w as usize);
            for (x, value) in samples.into_iter().enumerate() {
                let Some(value) = value else { continue };
                let top = (1. - (value / self.upper.max(1.)).clamp(0., 1.))
                    * (h.saturating_sub(1)) as f64;
                for y in 0..h {
                    let distance = (y as f64 - top).abs();
                    let pixel = image.get_pixel_mut(x as u32, y);
                    if y as f64 >= top {
                        let fade = 1. - (y as f64 - top) / (h as f64 - top).max(1.);
                        *pixel = mix(*pixel, rgba(color), 0.05 + opacity * fade);
                    }
                    if distance < 5. {
                        *pixel = mix(*pixel, rgba(color), 0.35 * (-distance / 1.8).exp());
                    }
                    if distance < 0.85 {
                        *pixel = mix(*pixel, rgba(color), 0.95);
                    }
                }
            }
        }
        image::DynamicImage::ImageRgba8(image)
    }
}

pub fn ceiling(value: f64) -> f64 {
    let value = value.max(1.0) * 1.15;
    let power = 10f64.powf(value.log10().floor());
    [1., 2., 5., 10.]
        .into_iter()
        .map(|v| v * power)
        .find(|v| *v >= value)
        .unwrap_or(value)
}

/// Interpolate short sampling intervals, retain spikes when downsampling, and
/// leave longer gaps blank rather than implying the connection was measured.
pub fn columns(points: &[(f64, f64)], start: f64, end: f64, width: usize) -> Vec<Option<f64>> {
    let mut result: Vec<Option<f64>> = vec![None; width];
    if width == 0 || end <= start {
        return result;
    }
    let scale = width.saturating_sub(1) as f64 / (end - start);
    let mut prev: Option<(f64, f64)> = None;
    for &(time, value) in points {
        if !time.is_finite() || !value.is_finite() || value < 0.0 {
            prev = None;
            continue;
        }
        if let Some((pt, pv)) = prev {
            if time > pt && time - pt <= 2.0 && time >= start && pt <= end {
                let left = ((pt.max(start) - start) * scale).ceil() as usize;
                let right = (((time.min(end) - start) * scale).floor() as usize).min(width - 1);
                for (x, out) in result.iter_mut().enumerate().take(right + 1).skip(left) {
                    let at = start + x as f64 / scale.max(f64::EPSILON);
                    let v = pv + (value - pv) * ((at - pt) / (time - pt)).clamp(0., 1.);
                    *out = Some(out.map_or(v, |old| old.max(v)));
                }
            }
        }
        if time >= start && time <= end {
            let x = (((time - start) * scale).round() as usize).min(width - 1);
            result[x] = Some(result[x].map_or(value, |old| old.max(value)));
        }
        prev = Some((time, value));
    }
    result
}
fn blend(a: Color, b: Color, amount: f64) -> Color {
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let mix = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * amount) as u8;
            Color::Rgb(mix(ar, br), mix(ag, bg), mix(ab, bb))
        }
        _ => b,
    }
}
pub struct History<'a> {
    pub primary: &'a [(f64, f64)],
    pub secondary: &'a [(f64, f64)],
    pub start: f64,
    pub end: f64,
    pub upper: f64,
    pub primary_color: Color,
    pub theme: Theme,
}
impl Widget for History<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let w = area.width as usize;
        let h = area.height as usize;
        let raster = |points| {
            let mut mask = vec![0u8; w * h];
            let mut edge = vec![false; w * h];
            for (x, value) in columns(points, self.start, self.end, w * 2)
                .into_iter()
                .enumerate()
            {
                let Some(value) = value else {
                    continue;
                };
                let top = ((1. - (value / self.upper.max(1.)).clamp(0., 1.)) * (h * 4 - 1) as f64)
                    .round() as usize;
                for y in top..h * 4 {
                    let bit = [[0, 3], [1, 4], [2, 5], [6, 7]][y % 4][x % 2];
                    mask[y / 4 * w + x / 2] |= 1 << bit;
                }
                edge[top / 4 * w + x / 2] = true;
            }
            (mask, edge)
        };
        let (a, ae) = raster(self.primary);
        let (b, be) = raster(self.secondary);
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let cell = &mut buf[(area.x + x as u16, area.y + y as u16)];
                cell.set_bg(self.theme.bg);
                let bits = a[i] | b[i];
                if bits != 0 {
                    let tint = if a[i] != 0 {
                        self.primary_color
                    } else {
                        self.theme.secondary
                    };
                    let strength = if ae[i] || be[i] {
                        0.95
                    } else {
                        0.24 + 0.40 * (1. - y as f64 / h as f64)
                    };
                    cell.set_char(char::from_u32(0x2800 + bits as u32).unwrap())
                        .set_fg(blend(self.theme.bg, tint, strength));
                } else {
                    cell.set_char(if y == h / 2 || y == h - 1 { '┄' } else { ' ' })
                        .set_fg(self.theme.edge);
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resampling_keeps_spikes_and_missing_intervals() {
        let data = [(0., 1.), (0.1, 100.), (0.2, 1.), (8., 3.)];
        let out = columns(&data, 0., 10., 10);
        assert_eq!(out[0], Some(100.));
        assert!(out[3..7].iter().all(Option::is_none));
        assert!(columns(&[], 0., 1., 10).iter().all(Option::is_none));
    }
    #[test]
    fn supports_single_columns_invalid_data_and_useful_scales() {
        assert_eq!(columns(&[(0., 8.)], 0., 1., 1), vec![Some(8.)]);
        assert_eq!(columns(&[(0., f64::NAN)], 0., 1., 2), vec![None, None]);
        assert_eq!(ceiling(42.), 50.);
        assert!(ceiling(128.) > 128.);
    }
}

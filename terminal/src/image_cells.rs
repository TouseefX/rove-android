//! Fit 32 image samples to quadrant and fractional block glyphs per cell.
//! Fine horizontal edges can use eighth blocks without dotted photo textures.
use image::{imageops, DynamicImage, Rgba, RgbaImage};
use ratatui::{buffer::Buffer, layout::Rect, style::Color, widgets::Widget};

pub struct Cells {
    width: u16,
    data: Vec<(char, Color, Color)>,
}
impl Cells {
    pub fn new(
        source: &DynamicImage,
        area: Rect,
        cell: (u16, u16),
        fill: bool,
        background: Rgba<u8>,
    ) -> Result<Self, String> {
        if area.is_empty() || u32::from(area.width) * u32::from(area.height) > 131_072 {
            return Err("artwork cell budget exceeded".into());
        }
        let size = (u32::from(area.width) * 4, u32::from(area.height) * 8);
        let physical = (
            f64::from(area.width) * f64::from(cell.0.max(1)),
            f64::from(area.height) * f64::from(cell.1.max(1)),
        );
        let mut canvas = RgbaImage::from_pixel(size.0, size.1, background);
        let image = if fill {
            // Crop in physical coordinates before sampling the non-square
            // quadrants, so circles stay circular in a tall terminal font.
            let ratio = physical.0 / physical.1;
            let w = source
                .width()
                .min((f64::from(source.height()) * ratio).round().max(1.) as u32);
            let h = source
                .height()
                .min((f64::from(source.width()) / ratio).round().max(1.) as u32);
            source
                .crop_imm((source.width() - w) / 2, (source.height() - h) / 2, w, h)
                .resize_exact(size.0, size.1, imageops::FilterType::Lanczos3)
                .to_rgba8()
        } else {
            let scale = (physical.0 / f64::from(source.width()))
                .min(physical.1 / f64::from(source.height()));
            let w = (f64::from(source.width()) * scale * 4. / f64::from(cell.0.max(1)))
                .round()
                .clamp(1., f64::from(size.0)) as u32;
            let h = (f64::from(source.height()) * scale * 8. / f64::from(cell.1.max(1)))
                .round()
                .clamp(1., f64::from(size.1)) as u32;
            source
                .resize_exact(w, h, imageops::FilterType::Lanczos3)
                .to_rgba8()
        };
        imageops::overlay(
            &mut canvas,
            &image,
            i64::from((size.0 - image.width()) / 2),
            i64::from((size.1 - image.height()) / 2),
        );
        let mut data = Vec::with_capacity(usize::from(area.width) * usize::from(area.height));
        for y in 0..area.height {
            for x in 0..area.width {
                let (x, y) = (u32::from(x) * 4, u32::from(y) * 8);
                data.push(fit_cell(std::array::from_fn(|i| {
                    canvas[(x + (i % 4) as u32, y + (i / 4) as u32)]
                })));
            }
        }
        Ok(Self {
            width: area.width,
            data,
        })
    }
}
// Retain every old quadrant partition and add thinner horizontal/vertical cuts.
// Complements are represented by swapping the two ink colors.
const fn candidates() -> [(char, u32); 16] {
    let glyphs = [
        ' ', '▘', '▝', '▀', '▖', '▌', '▞', '▛', '▁', '▂', '▃', '▅', '▆', '▇', '▎', '▊',
    ];
    let heights = [1, 2, 3, 5, 6, 7];
    let mut result = [(' ', 0); 16];
    let mut g = 0;
    while g < 16 {
        let mut mask = 0;
        let mut i = 0;
        while i < 32 {
            let (x, y) = (i % 4, i / 4);
            let on = if g < 8 {
                let quadrant = (if y >= 4 { 2 } else { 0 }) + if x >= 2 { 1 } else { 0 };
                g & (1 << quadrant) != 0
            } else if g < 14 {
                y >= 8 - heights[g - 8]
            } else {
                x < if g == 14 { 1 } else { 3 }
            };
            if on {
                mask |= 1 << i;
            }
            i += 1;
        }
        result[g] = (glyphs[g], mask);
        g += 1;
    }
    result
}
fn fit_cell(pixels: [Rgba<u8>; 32]) -> (char, Color, Color) {
    const SHAPES: [(char, u32); 16] = candidates();
    let mut totals = [0u32; 3];
    let mut squares = [0u32; 3];
    for pixel in &pixels {
        for c in 0..3 {
            let v = u32::from(pixel[c]);
            totals[c] += v;
            squares[c] += v * v;
        }
    }
    let mut best = (u64::MAX, ' ', [[0u8; 3]; 2]);
    for (glyph, mask) in SHAPES {
        let counts = [32 - mask.count_ones(), mask.count_ones()];
        let mut sums = [[0u32; 3]; 2];
        let mut remaining = mask;
        while remaining != 0 {
            let pixel = pixels[remaining.trailing_zeros() as usize];
            for c in 0..3 {
                sums[1][c] += u32::from(pixel[c]);
            }
            remaining &= remaining - 1;
        }
        for c in 0..3 {
            sums[0][c] = totals[c] - sums[1][c];
        }
        let colors: [[u8; 3]; 2] = std::array::from_fn(|group| {
            sums[group].map(|sum| ((sum + counts[group] / 2) / counts[group].max(1)) as u8)
        });
        let mut error = 0u64;
        for (c, weight) in [3, 6, 1].into_iter().enumerate() {
            let mut squared_error = i64::from(squares[c]);
            for group in 0..2 {
                let mean = i64::from(colors[group][c]);
                squared_error +=
                    i64::from(counts[group]) * mean * mean - 2 * mean * i64::from(sums[group][c]);
            }
            error += squared_error as u64 * weight;
        }
        if error < best.0 {
            best = (error, glyph, colors);
        }
    }
    let color = |v: [u8; 3]| Color::Rgb(v[0], v[1], v[2]);
    (best.1, color(best.2[1]), color(best.2[0]))
}
impl Widget for &Cells {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        for (index, &(symbol, fg, bg)) in self.data.iter().enumerate() {
            let x = (index % usize::from(self.width)) as u16;
            let y = (index / usize::from(self.width)) as u16;
            if x < area.width && y < area.height {
                if let Some(cell) = buffer.cell_mut((area.x + x, area.y + y)) {
                    cell.set_char(symbol).set_fg(fg).set_bg(bg);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn quadrants(pixels: [Rgba<u8>; 4]) -> (char, Color, Color) {
        fit_cell(std::array::from_fn(|i| {
            pixels[(i / 4 / 4) * 2 + (i % 4 / 2)]
        }))
    }
    #[test]
    fn eighth_height_and_quarter_width_edges_preserve_exact_colors() {
        let red = Rgba([240, 20, 30, 255]);
        let blue = Rgba([10, 40, 230, 255]);
        for (pixels, glyph) in [
            (
                std::array::from_fn(|i| if i / 4 == 7 { red } else { blue }),
                '▁',
            ),
            (
                std::array::from_fn(|i| if i % 4 == 0 { red } else { blue }),
                '▎',
            ),
        ] {
            assert_eq!(
                fit_cell(pixels),
                (glyph, Color::Rgb(240, 20, 30), Color::Rgb(10, 40, 230))
            );
        }
    }
    #[test]
    fn thin_vertical_diagonal_and_horizontal_edges_remain_distinct() {
        let a = Rgba([240, 20, 30, 255]);
        let b = Rgba([10, 40, 230, 255]);
        for (pixels, glyph) in [
            ([a, b, a, b], '▌'),
            ([a, a, b, b], '▀'),
            ([b, a, a, b], '▞'),
        ] {
            let (actual, fg, bg) = quadrants(pixels);
            assert_eq!(actual, glyph);
            assert_eq!(fg, Color::Rgb(240, 20, 30));
            assert_eq!(bg, Color::Rgb(10, 40, 230));
        }
        assert_eq!(quadrants([a; 4]).0, ' ');
    }
    #[test]
    fn square_logos_keep_physical_proportions_and_alpha_is_composited() {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(8, 8, Rgba([255, 0, 0, 128])));
        let cells = Cells::new(
            &image,
            Rect::new(0, 0, 8, 8),
            (10, 20),
            false,
            Rgba([0, 0, 0, 255]),
        )
        .unwrap();
        // An 80x160 physical slot contains an 80x80 square: four rows, centered.
        assert_eq!(
            cells
                .data
                .iter()
                .filter(|(_, _, bg)| *bg == Color::Rgb(128, 0, 0))
                .count(),
            32
        );
        assert_eq!(cells.data[0].2, Color::Rgb(0, 0, 0));
        assert!(Cells::new(
            &image,
            Rect::new(0, 0, 1000, 1000),
            (10, 20),
            true,
            Rgba([0, 0, 0, 255])
        )
        .is_err());
    }
}

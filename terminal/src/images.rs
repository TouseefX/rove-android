//! One bounded encoding worker; drawing never downloads or encodes an image.
use crate::{artwork::Loader, graph::Chart, theme::Theme};
use image::{imageops, DynamicImage, Rgba, RgbaImage};
use ratatui::{layout::Rect, style::Color, Frame};
use ratatui_image::{
    protocol::{halfblocks::Halfblocks, iterm2::Iterm2, kitty::Kitty, sixel::Sixel, Protocol},
    Image,
};
#[path = "image_cells.rs"]
mod image_cells;

enum EncodedImage {
    Pixels(Protocol),
    Cells(image_cells::Cells),
}
impl EncodedImage {
    fn draw(&self, frame: &mut Frame, area: Rect) {
        match self {
            Self::Pixels(image) => frame.render_widget(Image::new(image), area),
            Self::Cells(image) => frame.render_widget(image, area),
        }
    }
}
use std::{
    collections::HashMap,
    io::Write,
    sync::{mpsc, Arc, OnceLock},
    thread,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Blocks,
    Kitty,
    Iterm2,
    Sixel,
    Off,
}
impl Mode {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "auto" => Ok(Self::Auto),
            "blocks" => Ok(Self::Blocks),
            "off" => Ok(Self::Off),
            "kitty" => Ok(Self::Kitty),
            "iterm2" => Ok(Self::Iterm2),
            "sixel" => Ok(Self::Sixel),
            _ => Err("Images must be auto, blocks, off, kitty, iterm2, or sixel".into()),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Kitty | Self::Iterm2 | Self::Sixel => "pixel images",
            Self::Off => "images off",
            _ => "block images",
        }
    }
    fn pixels(self) -> bool {
        matches!(self, Self::Kitty | Self::Iterm2 | Self::Sixel)
    }
}
#[derive(Clone, Copy)]
pub struct Config {
    pub mode: Mode,
    pub cell: (u16, u16),
}
impl Config {
    pub fn detect(mode: Mode) -> Self {
        let mut config = Self {
            mode,
            cell: (10, 20),
        };
        // Avoid changing a multiplexer configuration. Blocks work without passthrough.
        let multiplexed = std::env::var_os("TMUX").is_some()
            || std::env::var_os("ZELLIJ").is_some()
            || std::env::var("TERM").is_ok_and(|term| term.starts_with("tmux"));
        if multiplexed && mode != Mode::Off {
            config.mode = Mode::Blocks;
            return config;
        }
        let metrics = crossterm::terminal::window_size()
            .ok()
            .filter(|w| w.columns > 0 && w.rows > 0 && w.width > 0 && w.height > 0);
        if let Some(window) = &metrics {
            config.cell = (window.width / window.columns, window.height / window.rows);
        }
        if mode == Mode::Auto {
            let program = std::env::var("TERM_PROGRAM").unwrap_or_default();
            let term = std::env::var("TERM").unwrap_or_default();
            let legacy = std::env::var("LC_TERMINAL").unwrap_or_default();
            config.mode = known_protocol(
                &program,
                &term,
                &legacy,
                std::env::var_os("WT_SESSION").is_some(),
                metrics.is_some(),
            );
        }
        config.cell.0 = config.cell.0.clamp(1, 64);
        config.cell.1 = config.cell.1.clamp(1, 128);
        config
    }
}
// Metadata detection never reads stdin: a timed-out probing thread can otherwise
// steal the user's first commands. Unknown terminals conservatively use blocks.
fn known_protocol(program: &str, term: &str, legacy: &str, windows: bool, metrics: bool) -> Mode {
    let program = program.to_ascii_lowercase();
    let term = term.to_ascii_lowercase();
    if program == "ghostty"
        || program == "kitty"
        || term == "xterm-kitty"
        || term == "xterm-ghostty"
    {
        Mode::Kitty
    } else if ["iterm.app", "iterm2", "wezterm", "rio", "tabby", "bobcat"]
        .contains(&program.as_str())
        || (program.is_empty() && legacy.eq_ignore_ascii_case("iterm2"))
    {
        Mode::Iterm2
    } else if metrics
        && (windows || term.contains("sixel") || ["foot", "mlterm"].contains(&program.as_str()))
    {
        Mode::Sixel
    } else {
        Mode::Blocks
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Slot {
    Brand,
    Welcome,
    Hero,
    Thumb0,
    Thumb1,
    Thumb2,
    GameIcon,
    Session,
    Memory,
    Network,
}
impl Slot {
    fn id(self) -> u32 {
        // Reuse a fixed ID per slot, so chart updates don't accumulate images in the terminal.
        ((std::process::id() & 0x000f_ffff) << 4) | (self as u32 + 1)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Key {
    pub source: u64,
    pub version: u64,
    pub theme: usize,
    pub width: u16,
    pub height: u16,
}
impl Key {
    pub fn new(source: u64, version: u64, theme: usize, area: Rect) -> Self {
        Self {
            source,
            version,
            theme,
            width: area.width,
            height: area.height,
        }
    }
    fn same_surface(self, other: Self) -> bool {
        self.source == other.source
            && self.theme == other.theme
            && self.width == other.width
            && self.height == other.height
    }
}
pub enum Source {
    Picture(Arc<DynamicImage>),
    Mark,
    Chart(Chart),
}
struct Job {
    slot: Slot,
    key: Key,
    source: Source,
    theme: Theme,
    config: Config,
}
struct Encoded {
    slot: Slot,
    key: Key,
    result: Result<EncodedImage, String>,
}
#[derive(Default)]
struct Surface {
    pending: Option<Key>,
    ready: Option<(Key, EncodedImage)>,
    failed: Option<Key>,
}
pub struct Images {
    pub art: Loader,
    pub config: Config,
    tx: Option<mpsc::SyncSender<Job>>,
    rx: Option<mpsc::Receiver<Encoded>>,
    surfaces: HashMap<Slot, Surface>,
}
impl Default for Images {
    fn default() -> Self {
        Self {
            art: Loader::default(),
            config: Config {
                mode: Mode::Off,
                cell: (10, 20),
            },
            tx: None,
            rx: None,
            surfaces: HashMap::new(),
        }
    }
}
impl Images {
    pub fn new(config: Config) -> Self {
        let mut images = Self {
            config,
            ..Self::default()
        };
        if config.mode == Mode::Off {
            return images;
        }
        let (tx, jobs) = mpsc::sync_channel::<Job>(8);
        let (done, rx) = mpsc::sync_channel(8);
        thread::spawn(move || {
            while let Ok(job) = jobs.recv() {
                let result = encode(&job);
                if done
                    .send(Encoded {
                        slot: job.slot,
                        key: job.key,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        images.tx = Some(tx);
        images.rx = Some(rx);
        images
    }
    pub fn enabled(&self) -> bool {
        self.config.mode != Mode::Off
    }
    pub fn pixels(&self) -> bool {
        self.config.mode.pixels()
    }
    pub fn poll(&mut self) {
        self.art.poll();
        while let Some(done) = self.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            let surface = self.surfaces.entry(done.slot).or_default();
            if surface.pending != Some(done.key) {
                continue;
            }
            surface.pending = None;
            match done.result {
                Ok(protocol) => {
                    surface.ready = Some((done.key, protocol));
                    surface.failed = None;
                }
                Err(_) => {
                    surface.failed = Some(done.key);
                }
            }
        }
    }
    pub fn draw(
        &mut self,
        frame: &mut Frame,
        slot: Slot,
        key: Key,
        area: Rect,
        theme: Theme,
        source: impl FnOnce() -> Source,
    ) -> bool {
        if !self.enabled() || area.is_empty() {
            return false;
        }
        let surface = self.surfaces.entry(slot).or_default();
        if surface.pending.is_none()
            && surface.failed != Some(key)
            && !surface
                .ready
                .as_ref()
                .is_some_and(|(ready, _)| *ready == key)
        {
            if let Some(tx) = &self.tx {
                if tx
                    .try_send(Job {
                        slot,
                        key,
                        source: source(),
                        theme,
                        config: self.config,
                    })
                    .is_ok()
                {
                    surface.pending = Some(key);
                }
            }
        }
        if let Some((ready, image)) = &surface.ready {
            if ready.same_surface(key) {
                image.draw(frame, area);
                return true;
            }
        }
        false
    }
    pub fn cleanup(&self) {
        if self.config.mode != Mode::Kitty {
            return;
        }
        let mut stdout = std::io::stdout().lock();
        for slot in self.surfaces.keys() {
            let _ = write!(stdout, "\x1b_Ga=d,d=I,i={},q=2\x1b\\", slot.id());
        }
        let _ = stdout.flush();
    }
}
pub fn rgba(color: Color) -> Rgba<u8> {
    match color {
        Color::Rgb(r, g, b) => Rgba([r, g, b, 255]),
        _ => Rgba([14, 19, 32, 255]),
    }
}
pub fn mix(a: Rgba<u8>, b: Rgba<u8>, weight: f64) -> Rgba<u8> {
    let w = weight.clamp(0., 1.);
    Rgba(std::array::from_fn(|i| {
        if i == 3 {
            255
        } else {
            (a[i] as f64 * (1. - w) + b[i] as f64 * w) as u8
        }
    }))
}
fn fit(source: &DynamicImage, size: (u32, u32), bg: Rgba<u8>) -> DynamicImage {
    let size = (size.0.max(1), size.1.max(1));
    let mut canvas = RgbaImage::from_pixel(size.0, size.1, bg);
    let image = source
        .resize(size.0, size.1, imageops::FilterType::Lanczos3)
        .to_rgba8();
    imageops::overlay(
        &mut canvas,
        &image,
        ((size.0 - image.width()) / 2) as i64,
        ((size.1 - image.height()) / 2) as i64,
    );
    DynamicImage::ImageRgba8(canvas)
}
/// Fill a cover slot edge to edge, cropping the middle instead of stretching
/// the artwork or leaving theme-colored bars around it.
fn cover(source: &DynamicImage, size: (u32, u32)) -> DynamicImage {
    let (width, height) = (size.0.max(1), size.1.max(1));
    source.resize_to_fill(width, height, imageops::FilterType::Lanczos3)
}
fn encode(job: &Job) -> Result<EncodedImage, String> {
    let area = Rect::new(0, 0, job.key.width, job.key.height);
    let desired = (
        area.width as u32 * job.config.cell.0 as u32,
        area.height as u32 * job.config.cell.1 as u32,
    );
    let mode = if job.config.mode.pixels() && (desired.0 > 1280 || desired.1 > 720) {
        Mode::Blocks
    } else {
        job.config.mode
    };
    if !mode.pixels() {
        let (source, fill, background) = match &job.source {
            Source::Picture(image) => (image.as_ref(), job.slot != Slot::GameIcon, rgba(job.theme.panel)),
            Source::Mark => (mark(job.theme), false, rgba(job.theme.bg)),
            // Chart fallback is normally rendered by the graph widget itself.
            Source::Chart(chart) => {
                let raster = chart.raster((u32::from(area.width).max(1), u32::from(area.height).max(1)*2), job.theme);
                return image_cells::Cells::new(&raster, area, job.config.cell, true, rgba(job.theme.panel)).map(EncodedImage::Cells);
            }
        };
        return image_cells::Cells::new(source, area, job.config.cell, fill, background).map(EncodedImage::Cells);
    }
    let cell = if mode.pixels() {
        job.config.cell
    } else {
        (1, 2)
    };
    let size = (
        (area.width as u32 * cell.0 as u32).clamp(1, 1280),
        (area.height as u32 * cell.1 as u32).clamp(1, 720),
    );
    let image = match &job.source {
        // Square game marks should stay whole; landscape cover art fills its
        // gallery frame edge to edge and gets a restrained center crop.
        Source::Picture(image) if job.slot == Slot::GameIcon => {
            fit(image, size, rgba(job.theme.panel))
        }
        Source::Picture(image) => cover(image, size),
        Source::Mark => fit(mark(job.theme), size, rgba(job.theme.bg)),
        Source::Chart(chart) => chart.raster(size, job.theme),
    };
    let result = match mode {
        Mode::Kitty => Kitty::new(image, area, job.slot.id(), false).map(Protocol::Kitty),
        Mode::Iterm2 => {
            let pixels = format!("width={}px;height={}px", image.width(), image.height());
            Iterm2::new(image, area, false).map(|mut protocol| {
                protocol.data = protocol.data.replace(
                    &pixels,
                    &format!(
                        "width={};height={};preserveAspectRatio=0",
                        area.width, area.height
                    ),
                );
                Protocol::ITerm2(protocol)
            })
        }
        Mode::Sixel => Sixel::new(image, area, false).map(Protocol::Sixel),
        _ => Halfblocks::new(image, area).map(Protocol::Halfblocks),
    };
    result.map(EncodedImage::Pixels).map_err(|e| e.to_string())
}

/// The user's original Rove icon is bundled verbatim; it never needs a network call.
pub fn mark(_theme: Theme) -> &'static DynamicImage {
    static LOGO: OnceLock<DynamicImage> = OnceLock::new();
    LOGO.get_or_init(|| {
        image::load_from_memory(include_bytes!("../assets/rove.webp"))
            .expect("bundled Rove icon must decode")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    #[test]
    fn artwork_covers_wide_and_tall_slots_without_distortion_or_letterboxing() {
        let mut source = image::RgbaImage::from_pixel(8, 4, Rgba([0, 0, 0, 255]));
        for y in 0..4 {
            for x in 4..8 {
                source.put_pixel(x, y, Rgba([255, 0, 0, 255]));
            }
        }
        let wide = cover(&DynamicImage::ImageRgba8(source.clone()), (4, 4)).to_rgba8();
        assert!(wide.pixels().all(|pixel| pixel[3] == 255));
        assert!(wide.get_pixel(0, 0)[0] < wide.get_pixel(3, 0)[0]);
        let tall = cover(&DynamicImage::ImageRgba8(source), (4, 8)).to_rgba8();
        assert_eq!(tall.dimensions(), (4, 8));
        assert!(tall.pixels().all(|pixel| pixel[3] == 255));
        assert!(tall.get_pixel(0, 4)[0] < tall.get_pixel(3, 4)[0]);
    }
    #[test]
    fn game_icons_keep_their_full_square_artwork() {
        let source =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(8, 8, Rgba([230, 90, 140, 255])));
        let image = fit(&source, (16, 8), rgba(crate::theme::THEMES[0].panel)).to_rgba8();
        assert_eq!(*image.get_pixel(0, 0), rgba(crate::theme::THEMES[0].panel));
        assert_eq!(*image.get_pixel(4, 0), Rgba([230, 90, 140, 255]));
    }
    #[test]
    fn metadata_detection_is_conservative_and_does_not_consume_input() {
        assert_eq!(
            known_protocol("Apple_Terminal", "xterm-256color", "", false, false),
            Mode::Blocks
        );
        assert_eq!(
            known_protocol("unknown", "xterm-256color", "iTerm2", false, true),
            Mode::Blocks
        );
        assert_eq!(
            known_protocol("ghostty", "xterm-ghostty", "", false, false),
            Mode::Kitty
        );
        assert_eq!(
            known_protocol("iTerm.app", "xterm-256color", "", false, false),
            Mode::Iterm2
        );
        assert_eq!(
            known_protocol("", "xterm-256color", "", true, false),
            Mode::Blocks
        );
        assert_eq!(
            known_protocol("", "xterm-256color", "", true, true),
            Mode::Sixel
        );
    }
    #[test]
    fn each_image_protocol_encodes_and_blocks_really_have_two_colors() {
        for mode in [Mode::Blocks, Mode::Kitty, Mode::Iterm2, Mode::Sixel] {
            let area = Rect::new(0, 0, 12, 6);
            let job = Job {
                slot: Slot::Brand,
                key: Key::new(0, 0, 0, area),
                source: Source::Mark,
                theme: crate::theme::THEMES[0],
                config: Config {
                    mode,
                    cell: (8, 16),
                },
            };
            let protocol = encode(&job).unwrap();
            let mut terminal = Terminal::new(TestBackend::new(16, 8)).unwrap();
            terminal
                .draw(|f| protocol.draw(f, area))
                .unwrap();
            let buffer = terminal.backend().buffer();
            if mode == Mode::Blocks {
                assert!(buffer
                    .content()
                    .iter()
                    .any(|c| c.symbol() != " " && c.fg != c.bg));
            } else {
                let text = buffer
                    .content()
                    .iter()
                    .map(|c| c.symbol())
                    .collect::<String>();
                assert!(text.contains('\x1b'));
            }
        }
    }
    #[test]
    fn stale_results_cannot_be_drawn_for_a_different_game_or_size() {
        let one = Key::new(1, 0, 0, Rect::new(0, 0, 40, 10));
        assert!(!one.same_surface(Key { source: 2, ..one }));
        assert!(!one.same_surface(Key { width: 30, ..one }));
        assert!(!one.same_surface(Key { theme: 1, ..one }));
        assert!(one.same_surface(Key { version: 1, ..one }));
    }
}

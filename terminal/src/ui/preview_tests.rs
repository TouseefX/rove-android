//! Human-reviewable terminal mockups exported as self-contained SVG files.
//! Run with `ROVE_PREVIEW_DIR=/tmp/rove-previews cargo test --manifest-path terminal/Cargo.toml \
//! export_visual_previews -- --ignored --nocapture`.
use super::*;
use crate::{
    app::{Log, Sample},
    search::Game,
};
use ratatui::{
    backend::TestBackend,
    style::{Color as TermColor, Modifier},
    Terminal,
};
use std::{fs, path::PathBuf};

fn color(color: TermColor) -> String {
    match color {
        TermColor::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        TermColor::Black => "#000000".into(),
        TermColor::Red => "#cd3131".into(),
        TermColor::Green => "#0dbc79".into(),
        TermColor::Yellow => "#e5e510".into(),
        TermColor::Blue => "#2472c8".into(),
        TermColor::Magenta => "#bc3fbc".into(),
        TermColor::Cyan => "#11a8cd".into(),
        TermColor::Gray => "#e5e5e5".into(),
        TermColor::DarkGray => "#666666".into(),
        TermColor::LightRed => "#f14c4c".into(),
        TermColor::LightGreen => "#23d18b".into(),
        TermColor::LightYellow => "#f5f543".into(),
        TermColor::LightBlue => "#3b8eea".into(),
        TermColor::LightMagenta => "#d670d6".into(),
        TermColor::LightCyan => "#29b8db".into(),
        TermColor::White => "#ffffff".into(),
        TermColor::Indexed(n) => format!("var(--ansi-{n}, #a0a0a0)"),
        TermColor::Reset => "#ffffff".into(),
    }
}
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn render_svg(app: &App, width: u16, height: u16, title: &str) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal
        .draw(|frame| draw(frame, app))
        .expect("render design preview");
    let buffer = terminal.backend().buffer();
    let mut svg = format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\" role=\"img\" aria-label=\"Design preview: {}\"><title>Design preview · {} · synthetic fixture</title><rect width=\"100%\" height=\"100%\" fill=\"#0e1320\"/><g font-family=\"ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace\" font-size=\"14px\" dominant-baseline=\"hanging\">", width as u32*9, height as u32*20, width as u32*9, height as u32*20, escape(title), escape(title));
    // Paint every background first so a two-column terminal glyph cannot be
    // covered by the background pass for its continuation cell.
    for y in 0..height {
        for x in 0..width {
            let cell = &buffer[(x, y)];
            let px = x as u32 * 9;
            let py = y as u32 * 20;
            let bg = color(cell.bg);
            if cell.bg != TermColor::Reset {
                svg.push_str(&format!(
                    "<rect x=\"{px}\" y=\"{py}\" width=\"9\" height=\"20\" fill=\"{bg}\"/>"
                ));
            }
        }
    }
    for y in 0..height {
        for x in 0..width {
            let cell = &buffer[(x, y)];
            let px = x as u32 * 9;
            let py = y as u32 * 20;
            let symbol = cell.symbol();
            if symbol.trim().is_empty() || symbol == " " {
                continue;
            }
            let mut attrs = String::new();
            if cell.modifier.contains(Modifier::BOLD) {
                attrs.push_str(" font-weight=\"700\"");
            }
            if cell.modifier.contains(Modifier::ITALIC) {
                attrs.push_str(" font-style=\"italic\"");
            }
            let mut deco = Vec::new();
            if cell.modifier.contains(Modifier::UNDERLINED) {
                deco.push("underline");
            }
            if cell.modifier.contains(Modifier::CROSSED_OUT) {
                deco.push("line-through");
            }
            if !deco.is_empty() {
                attrs.push_str(&format!(" text-decoration=\"{}\"", deco.join(" ")));
            }
            svg.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" fill=\"{}\"{}>{}</text>",
                px + 0,
                py + 2,
                color(cell.fg),
                attrs,
                escape(symbol)
            ));
        }
    }
    svg.push_str("</g></svg>\n");
    svg
}
fn fixture() -> App {
    let mut options = crate::backend::Options::default();
    options.credential_file = None;
    let mut app = App::new(options, None);
    app.picker = None;
    app.images = std::cell::RefCell::new(crate::images::Images::new(
        crate::images::Config::detect(crate::images::Mode::Off),
    ));
    app.telemetry = serde_json::json!({
        "phase":"preview","account":"DESIGN PREVIEW · SYNTHETIC DATA","luau_ready":true,
        "elapsed_seconds":372.0,"ping_ms":48.0,"ping_p50_ms":42.0,"ping_p95_ms":71.0,
        "sent_packets":4821,"received_packets":8934,"sent_bytes":1839200,"received_bytes":8874200,
        "rss_bytes":57_800_000,"peak_sampled_rss_bytes":63_200_000,"rtt_ms":48.0,
        "ping_requests":128,"ping_replies":128,"ping_pending":0,
        "place_id":123456789,"game_name":"Islands of Glass"
    });
    for i in 0..48 {
        let time = i as f64 * 0.5;
        app.samples.push_back(Sample {
            time,
            client_bytes: 51_000_000 + (i as u64 * 137_000) % 7_000_000,
            ui_bytes: 12_000_000 + (i as u64 * 41_000) % 1_800_000,
            rtt_ms: Some(38.0 + ((i * 7) % 17) as f64),
        });
    }
    for (level, text) in [
        ("info", "connected · Islands of Glass"),
        ("info", "received world snapshot · 1,284 instances"),
        ("result", "Luau VM ready · 14 scripts"),
        ("warn", "asset thumbnail unavailable · using fallback"),
        (
            "info",
            "DESIGN PREVIEW / SYNTHETIC DATA · no live Roblox session",
        ),
        ("info", "ping sample · 47 ms"),
    ] {
        app.logs.push_back(Log {
            at: 2.0,
            level,
            text: text.into(),
        });
    }
    app.input
        .set("local player = game:GetService(\"Players\").LocalPlayer\nprint(player.Name)".into());
    app
}
fn save(dir: &PathBuf, name: &str, svg: String) {
    fs::write(dir.join(format!("{name}.svg")), svg).expect("write design preview SVG");
}

#[test]
#[ignore = "manual visual-review artifact exporter; requires ROVE_PREVIEW_DIR"]
fn export_visual_previews() {
    let dir = PathBuf::from(std::env::var_os("ROVE_PREVIEW_DIR").expect("set ROVE_PREVIEW_DIR"));
    fs::create_dir_all(&dir).expect("create preview folder");
    let mut app = fixture();
    app.view = View::Overview;
    save(
        &dir,
        "overview-140x46",
        render_svg(&app, 140, 46, "overview 140x46"),
    );
    save(
        &dir,
        "overview-80x24",
        render_svg(&app, 80, 24, "overview 80x24"),
    );
    app.view = View::Console;
    save(
        &dir,
        "console-140x46",
        render_svg(&app, 140, 46, "console 140x46"),
    );
    app.view = View::Network;
    save(
        &dir,
        "network-140x46",
        render_svg(&app, 140, 46, "network 140x46"),
    );
    app.view = View::Overview;
    let picker = app.picker.get_or_insert_with(crate::app::Picker::default);
    picker.input.set("islands".into());
    picker.message = "sample results · screenshots are local design fixtures".into();
    picker.results=vec![Game {place_id:123456789,universe_id:Some(987654321),name:"Islands of Glass".into(),
        creator:"Rove Studio".into(),players:Some(1842),description:"DESIGN PREVIEW · SYNTHETIC DATA · no live Roblox session\n\nExplore a chain of luminous islands, build your home, and discover what the tide brings in.".into(),
        verified:true,approval:Some(96.0)}];
    picker.selected = 0;
    save(
        &dir,
        "game-picker-140x46",
        render_svg(&app, 140, 46, "game picker 140x46"),
    );
    app.picker.as_mut().unwrap().expanded_preview = true;
    save(
        &dir,
        "expanded-140x46",
        render_svg(&app, 140, 46, "expanded gallery"),
    );
    save(
        &dir,
        "expanded-80x24",
        render_svg(&app, 80, 24, "compact expanded gallery"),
    );
}

#[test]
fn svg_preview_encoder_escapes_fixture_text_and_marks_document_as_preview() {
    let app = fixture();
    let svg = render_svg(&app, 80, 24, "sample <preview>");
    assert!(svg.contains("Design preview · sample &lt;preview&gt; · synthetic fixture"));
    assert!(svg.starts_with("<svg "));
    assert!(svg.ends_with("</svg>\n"));
    assert!(svg.contains("width=\"720\" height=\"480\""));
    assert!(
        svg.matches("<text ").count() > 10,
        "preview should include actual rendered terminal glyphs"
    );
}

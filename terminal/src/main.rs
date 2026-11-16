mod actions;
mod app;
mod artwork;
mod backend;
mod editor;
mod fuzzy;
mod graph;
mod images;
// Reuse the small OS reader without compiling the world/transport libraries.
#[path = "../../client-core/src/memory.rs"]
#[allow(dead_code)]
mod memory;
// The shared budget module refers to this name for its process-memory reader.
// An embedded worker uses the actual core crate instead.
#[cfg(not(feature = "embedded-worker"))]
extern crate self as rove_client_core;
// Keep the shared budget policy without linking the VM into the display.
#[path = "../../rust-client/src/memory_budget.rs"]
#[allow(dead_code)]
mod memory_budget;
mod search;
mod syntax;
mod theme;
mod ui;

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

use backend::{Options, Target};
use std::{
    io::{self, IsTerminal},
    path::PathBuf,
};

const HELP: &str = "rove terminal — live RAM, game search, and Luau\n\n  terminal/run.sh [--credential-file FILE] [--place-id ID]\n  terminal/run.sh --offline\n\n  --seconds 1..3600       session duration (default: 3600)\n  --lua-memory-mib 8..512 Luau allocator cap (default: 32)\n  --no-physics           disable own-character physics\n  --lightweight          omit visual/audio asset properties\n  --search NAME          print public Roblox search results\n  --images MODE          auto (default), blocks, off; kitty/iterm2/sixel override\n\n  Ctrl+K actions · F2 search · Alt+1/2/3 views · Ctrl+T theme · Ctrl+Q quit\n  Enter runs Luau · Alt+Enter adds a line · :run PATH loads a script\n\nWhole-client RAM uses a 128 MB soft budget, growing by 10 MB with a 10 MB buffer.\nThis includes the dashboard and is separate from the Luau script cap.\n\nSet ROVE_COOKIE_FILE or pass --credential-file. Cookies are never typed in the UI.";

fn main() {
    if let Err(error) = run() {
        eprintln!("rove: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), String> {
    let mut options = Options::default();
    let mut target = None;
    let mut worker = false;
    let mut query = None;
    let mut image_mode = images::Mode::Auto;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{HELP}");
                return Ok(());
            }
            "--worker" => worker = true,
            "--images" => {
                image_mode = images::Mode::parse(&args.next().ok_or("Missing image mode")?)?
            }
            "--offline" => target = Some(Target::Sandbox),
            "--no-physics" => options.physics = false,
            "--lightweight" => options.lightweight = true,
            "--credential-file" => {
                options.credential_file = Some(PathBuf::from(
                    args.next().ok_or("Missing credential file path")?,
                ))
            }
            "--place-id" => {
                target = Some(Target::Game(
                    search::direct(&args.next().ok_or("Missing place ID")?)
                        .ok_or("Invalid place ID")?,
                ))
            }
            "--seconds" => {
                options.seconds = args
                    .next()
                    .ok_or("Missing duration")?
                    .parse()
                    .ok()
                    .filter(|v| (1..=3600).contains(v))
                    .ok_or("Duration must be 1..3600 seconds")?
            }
            "--lua-memory-mib" => {
                options.memory_mib = args
                    .next()
                    .ok_or("Missing memory limit")?
                    .parse()
                    .ok()
                    .filter(|v| (8..=512).contains(v))
                    .ok_or("Luau memory limit must be 8..512 MiB")?
            }
            "--search" => query = Some(args.next().ok_or("Missing search query")?),
            _ => return Err(format!("Unknown option {arg}. Use --help.")),
        }
    }
    if worker {
        return backend::worker(target.ok_or("Worker needs a target")?, options);
    }
    if let Some(query) = query {
        for game in search::search(&query)? {
            println!(
                "{}\t{}\t{} playing\t{}",
                game.place_id,
                game.name,
                game.players.unwrap_or(0),
                game.creator
            );
        }
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(
            "Open a terminal window to use the dashboard (or use --search / --help)".into(),
        );
    }
    let old_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ui::restore();
        old_hook(info);
    }));
    let mut terminal = ui::enter().map_err(|e| e.to_string())?;
    let _guard = ui::TerminalGuard;
    let config = images::Config::detect(image_mode);
    let mut app = app::App::new(options, target);
    app.images = std::cell::RefCell::new(images::Images::new(config));
    let result = app.run(&mut terminal).map_err(|e| e.to_string());
    app.images.borrow().cleanup();
    result
}

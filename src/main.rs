// SPDX-License-Identifier: GPL-3.0-or-later
// lifewall — Conway's Game of Life as a smooth wallpaper.
//
// The simulation ticks at --tick seconds per generation; rendering runs at
// --fps, interpolating every cell's colour continuously along its timeline:
//   birth:  background -> newborn        (over 1 generation)
//   youth:  newborn    -> mature         (over --fade generations)
//   death:  colour-at-death -> background (over --fade generations)
//
// Two front-ends over the same board.rs Engine: --layer draws straight onto a
// Wayland background surface (layer.rs); without it the board goes to the
// terminal as escape codes (term.rs), for previews and `kitten panel`.

mod board;
mod glyphs;
mod gpu;
mod layer;
mod power;
mod term;

use board::{parse_hex, Config};
use std::sync::atomic::{AtomicBool, Ordering};

pub static QUIT: AtomicBool = AtomicBool::new(false);
pub static WINCH: AtomicBool = AtomicBool::new(false);
pub static RESEED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(sig: libc::c_int) {
    if sig == libc::SIGWINCH {
        WINCH.store(true, Ordering::Relaxed);
    } else if sig == libc::SIGUSR1 {
        RESEED.store(true, Ordering::Relaxed); // on-demand board reset
    } else {
        QUIT.store(true, Ordering::Relaxed);
    }
}

fn parse_args() -> Config {
    let mut cfg = Config::default();
    let mut args = std::env::args().skip(1);
    let usage = "lifewall — Conway's Game of Life wallpaper\n\
        lifewall --layer [flags]   draw on the Wayland background layer itself\n\
        lifewall [flags]           draw in this terminal (previews, kitten panel)\n\n\
        --tick SECS     seconds per generation        (default 0.3)\n\
        --fps N         render frames per second      (default 15)\n\
        --fps-battery N frames per second on battery; 0 uses\n\
        \x20               --fps on battery too         (default 8)\n\
        --fade GENS     fade length in generations    (default 3)\n\
        --density F     seed fill fraction 0..1       (default 0.14)\n\
        --char S        glyph(s) for live cells; 2+ chars picks randomly\n\
        \x20               per cell        (default: printable ASCII)\n\
        --bg HEX        background colour             (default #121412)\n\
        --mature HEX    settled cell colour           (default #66744c)\n\
        --newborn HEX   birth flash colour            (default #87a540)\n\
        --glider-interval SECS  mean seconds between glider clusters;\n\
        \x20                       0 disables                   (default 90)\n\
        --font-family NAME  --layer: fontconfig family  (default ShureTechMono Nerd Font)\n\
        --font-size PT      --layer: cell size in points, like kitty's font_size (default 8)\n";
    while let Some(a) = args.next() {
        let mut val = |name: &str| {
            args.next().unwrap_or_else(|| {
                eprintln!("missing value for {name}");
                std::process::exit(2);
            })
        };
        match a.as_str() {
            "--tick" => cfg.tick = val("--tick").parse().unwrap_or(cfg.tick),
            "--fps" => cfg.fps = val("--fps").parse().unwrap_or(cfg.fps),
            "--fps-battery" => cfg.fps_battery = val("--fps-battery").parse().unwrap_or(cfg.fps_battery),
            "--fade" => cfg.fade = val("--fade").parse().unwrap_or(cfg.fade),
            "--density" => cfg.density = val("--density").parse().unwrap_or(cfg.density),
            "--char" => {
                // Control characters (e.g. a stray ESC) would inject raw escape
                // sequences into our own output stream, so drop them.
                let v: Vec<char> = val("--char").chars().filter(|c| !c.is_control()).collect();
                if !v.is_empty() {
                    cfg.glyphs = v;
                }
            }
            "--glider-interval" => {
                cfg.glider_interval = val("--glider-interval")
                    .parse()
                    .unwrap_or(cfg.glider_interval)
            }
            "--layer" => cfg.layer = true,
            "--font-family" => cfg.font_family = val("--font-family"),
            "--font-size" => cfg.font_size = val("--font-size").parse().unwrap_or(cfg.font_size),
            "--bg" => cfg.bg = parse_hex(&val("--bg")).unwrap_or(cfg.bg),
            "--mature" => cfg.mature = parse_hex(&val("--mature")).unwrap_or(cfg.mature),
            "--newborn" => cfg.newborn = parse_hex(&val("--newborn")).unwrap_or(cfg.newborn),
            "--help" | "-h" => {
                print!("{usage}");
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown flag {other}\n\n{usage}");
                std::process::exit(2);
            }
        }
    }
    cfg.tick = cfg.tick.max(0.01);
    cfg.fps = cfg.fps.clamp(1.0, 240.0);
    // 0 is the documented "don't throttle on battery" value, so it survives;
    // anything else gets --fps's range.
    if cfg.fps_battery != 0.0 {
        cfg.fps_battery = cfg.fps_battery.clamp(1.0, 240.0);
    }
    cfg.fade = cfg.fade.max(0.25);
    cfg.font_size = cfg.font_size.clamp(2.0, 72.0);
    cfg
}


fn main() {
    let cfg = parse_args();
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGWINCH, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGUSR1, on_signal as *const () as libc::sighandler_t);
    }
    if cfg.layer {
        std::process::exit(layer::run(cfg));
    }
    term::run(&cfg);
}

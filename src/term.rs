// SPDX-License-Identifier: GPL-3.0-or-later
// Terminal front-end: the board as truecolor escape codes, for previews and
// for `kitten panel`. Only cells whose quantized colour changed since the last
// frame are redrawn, so steady-state output stays small even at 30 fps.

use crate::board::{Board, Config, Engine};
use crate::{QUIT, RESEED, WINCH};
use std::io::Write;
use std::sync::atomic::Ordering;
use std::time::Instant;

fn term_size() -> (usize, usize) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0
            && ws.ws_col > 0
            && ws.ws_row > 0
        {
            return (ws.ws_col as usize, ws.ws_row as usize);
        }
    }
    let env = |k: &str, d: usize| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    (env("COLUMNS", 80), env("LINES", 24))
}

// Append redraw commands for every cell whose colour changed. SGR state
// persists across cursor moves, so fg is tracked across the whole frame.
fn render(
    board: &Board,
    gen_f: f64,
    cfg: &Config,
    prev: &mut [[u8; 3]],
    cur_fg: &mut Option<[u8; 3]>,
    buf: &mut String,
) {
    use std::fmt::Write as _;
    let bg = cfg.bg.to_u8();
    for y in 0..board.h {
        let mut in_run = false;
        for x in 0..board.w {
            let i = y * board.w + x;
            let col = board.color_at(i, gen_f, cfg);
            if col == prev[i] {
                in_run = false;
                continue;
            }
            prev[i] = col;
            if !in_run {
                let _ = write!(buf, "\x1b[{};{}H", y + 1, x + 1);
                in_run = true;
            }
            if col == bg {
                buf.push(' '); // spaces paint the (already set) background
            } else {
                if *cur_fg != Some(col) {
                    let _ = write!(buf, "\x1b[38;2;{};{};{}m", col[0], col[1], col[2]);
                    *cur_fg = Some(col);
                }
                buf.push(board.glyph_at(i, cfg));
            }
        }
    }
}

pub fn run(cfg: &Config) {
    let (mut w, mut h) = term_size();
    let mut engine = Engine::new(cfg, w, h);

    let bg = cfg.bg.to_u8();
    let mut prev = vec![bg; w * h];
    let mut cur_fg: Option<[u8; 3]> = None;
    let mut buf = String::with_capacity(1 << 16);

    let mut out = std::io::stdout().lock();
    let bg_sgr = format!("\x1b[48;2;{};{};{}m", bg[0], bg[1], bg[2]);
    let _ = write!(out, "\x1b[?25l{bg_sgr}\x1b[2J");

    let mut power = crate::power::Power::new(Instant::now());
    let mut next_frame = Instant::now();

    while !QUIT.load(Ordering::Relaxed) {
        // Resize: rebuild the board and repaint from scratch.
        if WINCH.swap(false, Ordering::Relaxed) {
            let (nw, nh) = term_size();
            if (nw, nh) != (w, h) {
                (w, h) = (nw, nh);
                engine.resize(cfg, w, h);
                prev = vec![bg; w * h];
                cur_fg = None;
                let _ = write!(out, "{bg_sgr}\x1b[2J");
            }
        }
        if RESEED.swap(false, Ordering::Relaxed) {
            engine.reseed_now(cfg);
        }

        let gen_f = engine.advance(cfg);
        buf.clear();
        render(&engine.board, gen_f, cfg, &mut prev, &mut cur_fg, &mut buf);
        if !buf.is_empty() && (write!(out, "{bg_sgr}{buf}").is_err() || out.flush().is_err()) {
            break; // panel closed under us
        }

        next_frame += power.frame(cfg);
        let now = Instant::now();
        if next_frame > now {
            std::thread::sleep(next_frame - now);
        } else {
            next_frame = now; // fell behind; don't try to catch up
        }
    }

    let _ = write!(out, "\x1b[0m\x1b[?25h\x1b[2J\x1b[H");
    let _ = out.flush();
}

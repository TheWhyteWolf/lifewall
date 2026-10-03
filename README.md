# lifewall

Conway's Game of Life as a smooth wallpaper. The simulation ticks at
a relaxed pace while rendering interpolates every cell's colour at 15 fps:
births fade in, the newborn flash melts into the mature tone, deaths dissolve
back into the background. Cells are drawn as random printable ASCII by
default; `--char` takes a whole string, and each cell picks one glyph from it
(stable until the cell dies and is reborn).

A single ~1 MB binary. `--layer` draws the board on the Wayland background
layer itself, on the GPU (EGL + GLES2); without it, the board goes to the
terminal as escape codes.

## Build

```sh
cargo build --release        # -> target/release/lifewall
```

## Run

As the wallpaper, on any compositor with wlr-layer-shell (niri, sway,
Hyprland, river, …):

```sh
lifewall --layer --font-family 'ShureTechMono Nerd Font' --font-size 8
```

Smaller `--font-size` = finer cells. One surface per output, each its own
board; outputs that come and go are followed. Fonts are found through
fontconfig, with a per-glyph fallback (kana needs a CJK font such as
noto-fonts-cjk), and are dropped once the glyphs are rasterized.

In any plain terminal it draws there instead, which is handy for previewing.
It also still runs inside `kitten panel --edge=background`, as it used to.

### Cost

Measured on a 3072x1920 panel at 30 fps, same flags, nothing covering it:

| | wallpaper CPU | niri CPU | anonymous memory |
|---|---|---|---|
| inside `kitten panel` | 25% (kitty) + 7% | 10% | 50 MB |
| `--layer` | 5% | 11% | 14 MB (mostly the GPU driver) |

Per frame the CPU only computes each cell's colour and glyph slot and uploads
that grid (4 bytes a cell); a shader draws the pixels. Frames follow the
compositor's frame callbacks, so a covered or powered-off output costs nothing,
and a frame where no cell changed is not drawn at all. CPU-drawn shm buffers
were tried first and cost more than kitty: niri holds an attached shm buffer
until another replaces it, and alternating buffers made it re-upload the whole
frame every time.

## Flags

```
--tick SECS     seconds per generation        (default 0.3)
--fps N         render frames per second      (default 15)
--fps-battery N frames per second while running on battery; 0 uses
                --fps on battery too         (default 8)
--fade GENS     fade length in generations    (default 3)
--density F     seed fill fraction 0..1       (default 0.14)
--char S        glyph(s) for live cells; 2+ chars picks randomly
                per cell        (default: printable ASCII)
--bg HEX        background colour             (default #121412)
--mature HEX    settled cell colour           (default #66744c)
--newborn HEX   birth flash colour            (default #87a540)
--glider-interval SECS  mean seconds between glider clusters;
                        0 disables                   (default 90)
--layer             draw on the Wayland background layer (GPU)
--font-family NAME  --layer: fontconfig family  (default ShureTechMono Nerd Font)
--font-size PT      --layer: cell size in points, like kitty's font_size (default 8)
```

In a terminal, pick `--char` glyphs that render at one column each, or they'll
smear into their neighbor — plain ASCII is safe, as are half-width katakana
(U+FF66-FF9D, e.g. `ｱｶﾀﾅ`); full-width kana/kanji are double-width in most
terminal fonts and will misalign the grid. `--layer` has no such limit: a
glyph wider than the cell is scaled down to fit it.

The board is a torus (gliders wrap). Every minute or two (randomized, see
`--glider-interval`) a small swarm of 1-3 gliders launches from a random edge
in a random diagonal heading, so the board keeps drifting even once the
ambient soup has settled into still lifes and oscillators — a gentler,
continuous alternative to a full reseed. If it still settles completely (e.g.
the gliders collide and die out) or nearly dies out, that's the backstop: it
crossfades into a fresh soup after ~20 s of no change.

## Sharing / binaries

Rust binaries are per-OS and per-architecture: build once per target
(`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`, …)
and hand that file out, or just share this directory — anyone with rust runs
`cargo build --release`. For a maximally portable Linux binary build against
musl: `cargo build --release --target x86_64-unknown-linux-musl`.

## Frame rate

Frame rate is the knob that matters, in both modes. In a terminal it is nearly
all of the cost: the renderer only emits cells whose quantized colour changed,
but the terminal still has to parse and redraw them, and that costs several
times what the simulation does. On a 2019 MacBook Pro the old kitty-panel
wallpaper measured ~13% of a core, about two thirds of it in kitty. `--layer`
is far cheaper per frame (see Cost above), but its frames still scale with fps.

Terminal output, measured on a 256x76 panel with the board settled:

| settings | output |
|---|---|
| `--fps 30 --tick 0.3` | ~1.0 MB/s |
| `--fps 15 --tick 0.2` | ~640 KB/s |
| `--fps 10 --tick 0.2` | ~350 KB/s |
| `--fps 10 --tick 0.5` | ~240 KB/s |

`--density` is not on that list on purpose: changing it from 0.41 to 0.14 moved
the steady-state figure by under half a percent. Life converges on a similar
population whatever soup it was seeded from, so density only really shows in
the first seconds after a reseed. Turn down fps, not density.

There is also a ceiling on useful fps: `blend()` quantizes each fade to 16
steps over `fade * tick` seconds, so past roughly `16 / (fade * tick)` fps most
cells produce a frame identical to the previous one and get diffed away — 30
fps at the default tick was paying about double for that.

`--fps-battery` is why lifewall reads `/sys/class/power_supply` itself: no
upower, no D-Bus, no helper daemon, just two small sysfs reads a minute. A
machine with no charger in sysfs is a desktop and never throttles.

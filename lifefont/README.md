# lifefont

The glyph layer shared by every life* component that draws text: lifenote,
lifelock, lifegreet, lifeconf's GUI and lifeshot.

It has the same four calls the components used to make on `fontdue`
(`from_bytes`, `metrics`, `horizontal_line_metrics`, `rasterize`), with the same
pixel conventions, so switching was an import change. Underneath it is
[`ab_glyph`](https://crates.io/crates/ab_glyph), which reads a glyph's outline
only when asked for it.

## Why

fontdue builds every glyph's outline when it loads a font. The Nerd font
LifeBranch uses has 11,307 glyphs, almost all icons the pure-text desktop never
draws, and parsing them cost each process about 37 MB of heap:

| lifenote `--selftest`, popups on screen | RssAnon |
|---|---|
| fontdue | 39.7 MB |
| lifefont | 2.8 MB |

The output is pixel-identical: lifenote's `--render-ppm` sample stack has no
pixel that differs by more than 16/255. The `matches_fontdue` test checks
metrics and per-glyph coverage against fontdue directly. It skips when the font
isn't installed, as in CI.

## Use

```toml
lifefont = { path = "../lifefont" }
```

```rust
let font = lifefont::Font::from_path("/usr/share/fonts/TTF/ShureTechMonoNerdFontMono-Regular.ttf")?;
let lm = font.horizontal_line_metrics(15.0).unwrap();
let (m, coverage) = font.rasterize('─', 15.0); // m.width * m.height bytes, top row first
```

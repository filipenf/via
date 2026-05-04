use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fontdue::{Font, FontSettings, Metrics};
use tracing::info;

use super::config::{TerminalConfig, TerminalTheme};
use super::render::blend_pixel;

pub(super) struct FontRenderer {
    font: Font,
    font_size: f32,
    baseline: isize,
    pub(super) theme: TerminalTheme,
    cache: HashMap<char, GlyphBitmap>,
}

struct GlyphBitmap {
    metrics: Metrics,
    bitmap: Vec<u8>,
}

impl FontRenderer {
    pub(super) fn new(config: &TerminalConfig) -> Result<Self> {
        let font_path = font_path(config).context("failed to find a terminal font")?;
        let font_bytes = std::fs::read(&font_path)
            .with_context(|| format!("failed to read font {}", font_path.display()))?;
        let font = Font::from_bytes(font_bytes, FontSettings::default()).map_err(|error| {
            anyhow::anyhow!("failed to load font {}: {error}", font_path.display())
        })?;

        info!(font = %font_path.display(), "loaded terminal font");

        Ok(Self {
            font,
            font_size: config.font_pixels,
            baseline: config.metrics.baseline,
            theme: config.theme.clone(),
            cache: HashMap::new(),
        })
    }

    pub(super) fn draw_char(
        &mut self,
        buffer: &mut [u32],
        width: usize,
        height: usize,
        x: usize,
        y: usize,
        ch: char,
        color: u32,
    ) {
        let baseline = y as isize + self.baseline;
        let glyph = self.glyph(ch);
        let draw_x = x as isize + glyph.metrics.xmin as isize;
        let draw_y = baseline - glyph.metrics.ymin as isize - glyph.metrics.height as isize;

        for glyph_y in 0..glyph.metrics.height {
            for glyph_x in 0..glyph.metrics.width {
                let alpha = glyph.bitmap[glyph_y * glyph.metrics.width + glyph_x];

                if alpha == 0 {
                    continue;
                }

                blend_pixel(
                    buffer,
                    width,
                    height,
                    draw_x + glyph_x as isize,
                    draw_y + glyph_y as isize,
                    color,
                    alpha,
                );
            }
        }
    }

    fn glyph(&mut self, ch: char) -> &GlyphBitmap {
        self.cache.entry(ch).or_insert_with(|| {
            let (metrics, bitmap) = self.font.rasterize(ch, self.font_size);

            GlyphBitmap { metrics, bitmap }
        })
    }
}

fn font_path(config: &TerminalConfig) -> Option<PathBuf> {
    if let Some(path) = config.font_path.clone() {
        return Some(path);
    }

    if let Some(path) = config.font_family.as_deref().and_then(font_path_for_family) {
        return Some(path);
    }

    [
        "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
        "/usr/share/fonts/TTF/CaskaydiaMonoNerdFont-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    ]
    .into_iter()
    .map(Path::new)
    .find(|path| path.exists())
    .map(Path::to_path_buf)
}

fn font_path_for_family(family: &str) -> Option<PathBuf> {
    let family = family.to_ascii_lowercase().replace([' ', '-'], "");
    let candidates = [
        (
            ["jetbrainsmononerdfont", "jetbrainsmono"],
            [
                "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
                "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
            ],
        ),
        (
            ["caskaydiamono", "cascadiamono"],
            [
                "/usr/share/fonts/TTF/CaskaydiaMonoNerdFont-Regular.ttf",
                "/usr/share/fonts/TTF/CascadiaMono.ttf",
            ],
        ),
    ];

    for (names, paths) in candidates {
        if !names.iter().any(|name| family.contains(name)) {
            continue;
        }

        if let Some(path) = paths.into_iter().map(Path::new).find(|path| path.exists()) {
            return Some(path.to_path_buf());
        }
    }

    None
}

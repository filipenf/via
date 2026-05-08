use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fontdue::{Font, FontSettings, Metrics};
use tracing::info;

use super::config::{TerminalConfig, TerminalTheme};

pub(super) struct FontRenderer {
    font: Font,
    bold_font: Option<Font>,
    italic_font: Option<Font>,
    bold_italic_font: Option<Font>,
    font_size: f32,
    baseline: isize,
    pub(super) theme: TerminalTheme,
    cache: HashMap<GlyphKey, GlyphBitmap>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct GlyphKey {
    ch: char,
    bold: bool,
    italic: bool,
}

struct GlyphBitmap {
    metrics: Metrics,
    bitmap: Vec<u8>,
}

impl FontRenderer {
    pub(super) fn new(config: &TerminalConfig) -> Result<Self> {
        let font_path = font_path(config).context("failed to find a terminal font")?;
        let font = load_font(&font_path)?;
        let bold_font = font_variant_path(config, FontVariant::Bold)
            .filter(|path| path != &font_path)
            .map(|path| load_font(&path))
            .transpose()?;
        let italic_font = font_variant_path(config, FontVariant::Italic)
            .filter(|path| path != &font_path)
            .map(|path| load_font(&path))
            .transpose()?;
        let bold_italic_font = font_variant_path(config, FontVariant::BoldItalic)
            .filter(|path| path != &font_path)
            .map(|path| load_font(&path))
            .transpose()?;

        info!(font = %font_path.display(), "loaded terminal font");

        Ok(Self {
            font,
            bold_font,
            italic_font,
            bold_italic_font,
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
        bold: bool,
        italic: bool,
    ) {
        let baseline = y as isize + self.baseline;
        let glyph = self.glyph(ch, bold, italic);
        let draw_x = x as isize + glyph.metrics.xmin as isize;
        let draw_y = baseline - glyph.metrics.ymin as isize - glyph.metrics.height as isize;
        let start_x = draw_x.max(0) as usize;
        let start_y = draw_y.max(0) as usize;
        let end_x = (draw_x + glyph.metrics.width as isize).clamp(0, width as isize) as usize;
        let end_y = (draw_y + glyph.metrics.height as isize).clamp(0, height as isize) as usize;

        if start_x >= end_x || start_y >= end_y {
            return;
        }

        for screen_y in start_y..end_y {
            let glyph_y = (screen_y as isize - draw_y) as usize;
            let glyph_row = glyph_y * glyph.metrics.width;
            let buffer_row = screen_y * width;

            for screen_x in start_x..end_x {
                let glyph_x = (screen_x as isize - draw_x) as usize;
                let alpha = glyph.bitmap[glyph_row + glyph_x];

                if alpha == 0 {
                    continue;
                }

                let index = buffer_row + screen_x;
                if alpha == 255 {
                    buffer[index] = color;
                    continue;
                }

                let dst = buffer[index];
                let alpha = alpha as u32;
                let inv_alpha = 255 - alpha;
                let r = (((color >> 16) & 0xff) * alpha + ((dst >> 16) & 0xff) * inv_alpha) / 255;
                let g = (((color >> 8) & 0xff) * alpha + ((dst >> 8) & 0xff) * inv_alpha) / 255;
                let b = ((color & 0xff) * alpha + (dst & 0xff) * inv_alpha) / 255;
                buffer[index] = (r << 16) | (g << 8) | b;
            }
        }
    }

    fn glyph(&mut self, ch: char, bold: bool, italic: bool) -> &GlyphBitmap {
        let key = GlyphKey { ch, bold, italic };

        if !self.cache.contains_key(&key) {
            let (metrics, bitmap) = self.styled_font(bold, italic).rasterize(ch, self.font_size);
            self.cache.insert(key, GlyphBitmap { metrics, bitmap });
        }

        self.cache
            .get(&key)
            .expect("glyph cache should contain key")
    }

    fn styled_font(&self, bold: bool, italic: bool) -> &Font {
        match (bold, italic) {
            (true, true) => self
                .bold_italic_font
                .as_ref()
                .or(self.bold_font.as_ref())
                .or(self.italic_font.as_ref())
                .unwrap_or(&self.font),
            (true, false) => self.bold_font.as_ref().unwrap_or(&self.font),
            (false, true) => self.italic_font.as_ref().unwrap_or(&self.font),
            (false, false) => &self.font,
        }
    }
}

fn load_font(path: &Path) -> Result<Font> {
    let font_bytes =
        std::fs::read(path).with_context(|| format!("failed to read font {}", path.display()))?;
    Font::from_bytes(font_bytes, FontSettings::default())
        .map_err(|error| anyhow::anyhow!("failed to load font {}: {error}", path.display()))
}

#[derive(Clone, Copy)]
enum FontVariant {
    Regular,
    Bold,
    Italic,
    BoldItalic,
}

fn font_path(config: &TerminalConfig) -> Option<PathBuf> {
    if let Some(path) = config.font_path.clone() {
        return Some(path);
    }

    if let Some(path) = config
        .font_family
        .as_deref()
        .and_then(|family| font_path_for_family(family, FontVariant::Regular))
    {
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

fn font_variant_path(config: &TerminalConfig, variant: FontVariant) -> Option<PathBuf> {
    config
        .font_family
        .as_deref()
        .and_then(|family| font_path_for_family(family, variant))
}

fn font_path_for_family(family: &str, variant: FontVariant) -> Option<PathBuf> {
    let family = family.to_ascii_lowercase().replace([' ', '-'], "");

    let paths = if family.contains("jetbrainsmononerdfont") || family.contains("jetbrainsmono") {
        jetbrains_mono_paths(variant)
    } else if family.contains("caskaydiamono") || family.contains("cascadiamono") {
        caskaydia_mono_paths(variant)
    } else {
        return None;
    };

    if let Some(path) = paths.into_iter().map(Path::new).find(|path| path.exists()) {
        return Some(path.to_path_buf());
    }

    None
}

fn jetbrains_mono_paths(variant: FontVariant) -> [&'static str; 2] {
    match variant {
        FontVariant::Regular => [
            "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
            "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
        ],
        FontVariant::Bold => [
            "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Bold.ttf",
            "/usr/share/fonts/TTF/JetBrainsMono-Bold.ttf",
        ],
        FontVariant::Italic => [
            "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Italic.ttf",
            "/usr/share/fonts/TTF/JetBrainsMono-Italic.ttf",
        ],
        FontVariant::BoldItalic => [
            "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-BoldItalic.ttf",
            "/usr/share/fonts/TTF/JetBrainsMono-BoldItalic.ttf",
        ],
    }
}

fn caskaydia_mono_paths(variant: FontVariant) -> [&'static str; 2] {
    match variant {
        FontVariant::Regular => [
            "/usr/share/fonts/TTF/CaskaydiaMonoNerdFont-Regular.ttf",
            "/usr/share/fonts/TTF/CascadiaMono-Regular.ttf",
        ],
        FontVariant::Bold => [
            "/usr/share/fonts/TTF/CaskaydiaMonoNerdFont-Bold.ttf",
            "/usr/share/fonts/TTF/CascadiaMono-Bold.ttf",
        ],
        FontVariant::Italic => [
            "/usr/share/fonts/TTF/CaskaydiaMonoNerdFont-Italic.ttf",
            "/usr/share/fonts/TTF/CascadiaMono-Italic.ttf",
        ],
        FontVariant::BoldItalic => [
            "/usr/share/fonts/TTF/CaskaydiaMonoNerdFont-BoldItalic.ttf",
            "/usr/share/fonts/TTF/CascadiaMono-BoldItalic.ttf",
        ],
    }
}

use std::collections::HashMap;

use anyhow::Result;
use cosmic_text::{
    Attrs, Buffer, Family, FontSystem, Hinting, Metrics, Shaping, Style, SwashCache, SwashContent,
    Weight, fontdb,
};
use tracing::info;

use super::config::{TerminalConfig, TerminalTheme};

pub(super) struct FontRenderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    font_family: Option<String>,
    font_size: f32,
    cell_width: f32,
    line_height: f32,
    baseline: f32,
    hinting: Hinting,
    shaping: Shaping,
    coverage_boost: f32,
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
    left: i32,
    top: i32,
    width: usize,
    height: usize,
    bitmap: Vec<u8>,
}

impl FontRenderer {
    pub(super) fn new(config: &TerminalConfig) -> Result<Self> {
        let font_sources = config.font_path.clone().map(fontdb::Source::File);
        let font_system = FontSystem::new_with_fonts(font_sources);

        if let Some(font_family) = &config.font_family {
            info!(font_family, "using configured terminal font family");
        } else if let Some(font_path) = &config.font_path {
            info!(font = %font_path.display(), "using configured terminal font file");
        } else {
            info!("using system monospace terminal font");
        }

        let hinting = config.hinting;
        let shaping = config.shaping;
        let coverage_boost = config.glyph_coverage_boost;

        info!(
            font_size = config.font_size,
            font_pixels = config.font_pixels,
            cell_width = config.metrics.cell_width,
            cell_height = config.metrics.cell_height,
            baseline = config.metrics.baseline,
            ?hinting,
            ?shaping,
            coverage_boost,
            "terminal font renderer initialized"
        );

        Ok(Self {
            font_system,
            swash_cache: SwashCache::new(),
            font_family: config.font_family.clone(),
            font_size: config.font_pixels,
            cell_width: config.metrics.cell_width as f32,
            line_height: config.metrics.cell_height as f32,
            baseline: config.metrics.baseline as f32,
            hinting,
            shaping,
            coverage_boost,
            theme: config.theme.clone(),
            cache: HashMap::new(),
        })
    }

    #[allow(clippy::too_many_arguments)]
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
        let glyph = self.glyph(ch, bold, italic);
        let draw_x = x as i32 + glyph.left;
        let draw_y = y as i32 + glyph.top;
        let start_x = draw_x.max(0) as usize;
        let start_y = draw_y.max(0) as usize;
        let end_x = (draw_x + glyph.width as i32).clamp(0, width as i32) as usize;
        let end_y = (draw_y + glyph.height as i32).clamp(0, height as i32) as usize;

        if start_x >= end_x || start_y >= end_y {
            return;
        }

        for screen_y in start_y..end_y {
            let glyph_y = (screen_y as i32 - draw_y) as usize;
            let glyph_row = glyph_y * glyph.width;
            let buffer_row = screen_y * width;

            for screen_x in start_x..end_x {
                let glyph_x = (screen_x as i32 - draw_x) as usize;
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
            let glyph = self.render_glyph(ch, bold, italic);
            self.cache.insert(key, glyph);
        }

        self.cache
            .get(&key)
            .expect("glyph cache should contain key")
    }

    fn render_glyph(&mut self, ch: char, bold: bool, italic: bool) -> GlyphBitmap {
        let metrics = Metrics::new(self.font_size, self.line_height);
        let font_family = self.font_family.clone();
        let attrs = attrs(font_family.as_deref(), bold, italic);
        let text = ch.to_string();
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        let mut buffer = buffer.borrow_with(&mut self.font_system);
        buffer.set_hinting(self.hinting);
        buffer.set_size(Some(self.cell_width), Some(self.line_height));
        buffer.set_text(&text, &attrs, self.shaping, None);

        let physical = {
            let mut runs = buffer.layout_runs();
            runs.next()
                .and_then(|run| run.glyphs.first())
                .map(|glyph| glyph.physical((0.0, self.baseline - glyph.y), 1.0))
        };
        if let Some(physical) = physical {
            if let Some(image) = self
                .swash_cache
                .get_image_uncached(&mut self.font_system, physical.cache_key)
            {
                let width = image.placement.width as usize;
                let height = image.placement.height as usize;
                let mut bitmap: Vec<u8> = match image.content {
                    SwashContent::Mask => image.data,
                    SwashContent::Color => image.data.chunks_exact(4).map(|rgba| rgba[3]).collect(),
                    SwashContent::SubpixelMask => {
                        image.data.chunks_exact(4).map(|rgba| rgba[1]).collect()
                    }
                };
                boost_glyph_coverage(&mut bitmap, self.coverage_boost);

                return GlyphBitmap {
                    left: physical.x + image.placement.left,
                    top: physical.y - image.placement.top,
                    width,
                    height,
                    bitmap,
                };
            }
        }

        GlyphBitmap {
            left: 0,
            top: 0,
            width: 0,
            height: 0,
            bitmap: Vec::new(),
        }
    }
}

fn attrs(font_family: Option<&str>, bold: bool, italic: bool) -> Attrs<'_> {
    let family = font_family.map(Family::Name).unwrap_or(Family::Monospace);

    Attrs::new()
        .family(family)
        .weight(if bold { Weight::BOLD } else { Weight::NORMAL })
        .style(if italic { Style::Italic } else { Style::Normal })
}

fn boost_glyph_coverage(bitmap: &mut [u8], factor: f32) {
    if factor <= 0.0 {
        return;
    }

    for alpha in bitmap {
        if *alpha == 0 || *alpha == 255 {
            continue;
        }

        let alpha_f32 = *alpha as f32;
        let boost = alpha_f32 * (255.0 - alpha_f32) * factor / 255.0;
        *alpha = (alpha_f32 + boost).min(255.0) as u8;
    }
}

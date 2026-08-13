use std::collections::HashMap;

use anyhow::Result;
use cosmic_text::{
    Attrs, Buffer, Family, FontSystem, Hinting, Metrics, Shaping, Style, SwashCache, SwashContent,
    Weight, fontdb,
};
use tracing::info;

use super::config::{MIN_CELL_WIDTH, TerminalConfig, TerminalMetrics, TerminalTheme, env_f32};

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
        let mut font_system = FontSystem::new_with_fonts(font_sources);
        let font_family = resolve_font_family(&mut font_system, config);

        let hinting = config.hinting;
        let shaping = config.shaping;
        let coverage_boost = config.glyph_coverage_boost;

        info!(
            font_family = font_family.as_deref().unwrap_or("generic monospace"),
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
            font_family,
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

    /// Replace the heuristic cell width with the font's actual ASCII advance.
    ///
    /// The 10px-at-16px heuristic is calibrated for typical Linux monospace
    /// fonts. On macOS (and with Nerd Fonts) the advance can differ enough that
    /// glyphs sit in the left half of each cell.
    pub(super) fn apply_measured_cell_width(&mut self) -> TerminalMetrics {
        let cell_width_scale = env_f32("VIA_CELL_WIDTH_SCALE").unwrap_or(1.0).max(0.1);
        if let Some(advance) = self
            .measure_char_advance('0')
            .or_else(|| self.measure_char_advance('M'))
        {
            let measured = (advance * cell_width_scale)
                .round()
                .max(MIN_CELL_WIDTH as f32) as usize;
            if measured != self.cell_width.round() as usize {
                info!(
                    heuristic_cell_width = self.cell_width,
                    measured_advance = advance,
                    measured_cell_width = measured,
                    "using measured font advance for terminal cell width"
                );
            }
            self.cell_width = measured as f32;
            self.cache.clear();
        }

        TerminalMetrics {
            cell_width: self.cell_width.round() as usize,
            cell_height: self.line_height.round() as usize,
            baseline: self.baseline.round() as isize,
        }
    }

    fn measure_char_advance(&mut self, ch: char) -> Option<f32> {
        let metrics = Metrics::new(self.font_size, self.line_height);
        let font_family = self.font_family.clone();
        let attrs = attrs(font_family.as_deref(), false, false);
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        let mut buffer = buffer.borrow_with(&mut self.font_system);
        buffer.set_hinting(self.hinting);
        // Do not wrap to the heuristic cell width — that is what we are measuring.
        buffer.set_size(Some(self.font_size * 4.0), Some(self.line_height));
        buffer.set_text(&ch.to_string(), &attrs, self.shaping, None);
        buffer
            .layout_runs()
            .next()
            .and_then(|run| run.glyphs.first())
            .map(|glyph| glyph.w)
            .filter(|advance| *advance > 0.0)
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
        // Layout in a wide box so the glyph is not squashed to the cell width.
        buffer.set_size(Some(self.font_size * 4.0), Some(self.line_height));
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

/// Pick a font cosmic-text can actually load.
///
/// Ghostty's `font-family` is used when that face is installed. Otherwise we
/// must not keep `Family::Name("Missing Font")`: cosmic-text then falls back
/// to proportional `.SF NS` / "System Font", and measuring `M` for cell width
/// makes every narrower letter sit in the left half of its cell.
fn resolve_font_family(font_system: &mut FontSystem, config: &TerminalConfig) -> Option<String> {
    if let Some(path) = &config.font_path {
        info!(font = %path.display(), "using configured terminal font file");
    }

    if let Some(requested) = config.font_family.as_deref() {
        if let Some(canonical) = installed_family_name(font_system, requested) {
            font_system.db_mut().set_monospace_family(&canonical);
            info!(font_family = %canonical, "using configured terminal font family");
            return Some(canonical);
        }
        tracing::warn!(
            font_family = requested,
            "configured Ghostty font family not found; falling back to system monospace"
        );
    }

    for candidate in platform_monospace_families() {
        if let Some(canonical) = installed_family_name(font_system, candidate) {
            font_system.db_mut().set_monospace_family(&canonical);
            info!(font_family = %canonical, "using system monospace terminal font");
            return Some(canonical);
        }
    }

    info!("using generic monospace terminal font");
    None
}

fn installed_family_name(font_system: &FontSystem, wanted: &str) -> Option<String> {
    font_system.db().faces().find_map(|face| {
        face.families
            .iter()
            .find_map(|(name, _)| name.eq_ignore_ascii_case(wanted).then(|| name.clone()))
    })
}

fn platform_monospace_families() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        // cosmic-text defaults Family::Monospace to "Noto Sans Mono", which is
        // not a macOS system font. Menlo / SF Mono are.
        &["Menlo", ".SF NS Mono", "SF Mono", "Monaco", "Courier New"]
    }
    #[cfg(not(target_os = "macos"))]
    {
        &[
            "Noto Sans Mono",
            "DejaVu Sans Mono",
            "Liberation Mono",
            "FreeMono",
        ]
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::ghostty::config::TerminalConfig;

    #[test]
    fn measured_cell_width_matches_ascii_advance() {
        let mut config = TerminalConfig::default();
        config.finalize_metrics_for_scale(2.0);
        let mut renderer = FontRenderer::new(&config).unwrap();
        let Some(advance) = renderer.measure_char_advance('0') else {
            panic!("failed to measure 0 advance");
        };
        let metrics = renderer.apply_measured_cell_width();
        assert!(
            (metrics.cell_width as f32 - advance).abs() < 1.5,
            "cell width {} should match 0 advance {advance}",
            metrics.cell_width
        );
        assert!(metrics.cell_width >= MIN_CELL_WIDTH);
    }

    #[test]
    fn loads_dotfiles_ghostty_font_family_on_macos() {
        let config = TerminalConfig::load();
        if config.font_family.as_deref() == Some("Mononoki Nerd Font Mono") {
            return;
        }
        // Fine if the user has no Ghostty dotfiles; the macOS Application
        // Support stub must not block ~/.config/ghostty/config when it exists.
        let dotfiles = dirs_home()
            .map(|home| home.join(".config/ghostty/config"))
            .filter(|path| path.is_file());
        if let Some(path) = dotfiles {
            let contents = std::fs::read_to_string(path).unwrap();
            if contents.contains("font-family") {
                assert_eq!(
                    config.font_family.as_deref(),
                    Some("Mononoki Nerd Font Mono"),
                    "expected ~/.config/ghostty/config to override the empty macOS template"
                );
            }
        }
    }

    #[test]
    fn missing_configured_family_uses_monospace_not_system_font() {
        let mut config = TerminalConfig::default();
        config.font_family = Some("Mononoki Nerd Font Mono".into());
        config.finalize_metrics_for_scale(2.0);
        let mut renderer = FontRenderer::new(&config).unwrap();

        let (family, monospaced, m_advance) = glyph_face(&mut renderer, 'M');
        let (_, _, i_advance) = glyph_face(&mut renderer, 'i');
        assert!(
            monospaced,
            "ASCII should come from a monospace face, not {family:?}"
        );
        assert_ne!(
            family, "System Font",
            "missing Ghostty font-family must not fall back to proportional System Font"
        );
        assert!(
            (m_advance - i_advance).abs() < 1.5,
            "{family} should be monospace: M advance {m_advance} vs i {i_advance}"
        );

        let metrics = renderer.apply_measured_cell_width();
        assert!(
            (metrics.cell_width as f32 - m_advance).abs() < 1.5,
            "cell width {} should match {family} advance {m_advance}",
            metrics.cell_width
        );
    }

    fn dirs_home() -> Option<std::path::PathBuf> {
        std::env::var_os("HOME").map(std::path::PathBuf::from)
    }

    fn glyph_face(renderer: &mut FontRenderer, ch: char) -> (String, bool, f32) {
        let metrics = Metrics::new(renderer.font_size, renderer.line_height);
        let font_family = renderer.font_family.clone();
        let attrs = attrs(font_family.as_deref(), false, false);
        let (font_id, advance) = {
            let mut buffer = Buffer::new(&mut renderer.font_system, metrics);
            let mut buffer = buffer.borrow_with(&mut renderer.font_system);
            buffer.set_hinting(renderer.hinting);
            buffer.set_size(Some(renderer.font_size * 4.0), Some(renderer.line_height));
            buffer.set_text(&ch.to_string(), &attrs, renderer.shaping, None);
            let glyph = buffer
                .layout_runs()
                .next()
                .and_then(|run| run.glyphs.first().cloned())
                .expect("glyph");
            (glyph.font_id, glyph.w)
        };
        let face = renderer.font_system.db().face(font_id);
        let family = face
            .and_then(|face| face.families.first().map(|(name, _)| name.clone()))
            .unwrap_or_else(|| "?".into());
        let monospaced = face.map(|face| face.monospaced).unwrap_or(false);
        (family, monospaced, advance)
    }
}

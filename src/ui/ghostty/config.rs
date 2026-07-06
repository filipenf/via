use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cosmic_text::{Hinting, Shaping};
use tracing::debug;

pub(super) const DEFAULT_CELL_WIDTH: usize = 10;
pub(super) const DEFAULT_CELL_HEIGHT: usize = 22;
const DEFAULT_FONT_SIZE_POINTS: f32 = 12.0;
const DEFAULT_FONT_DPI: f32 = 96.0;
const DEFAULT_FONT_PIXEL_SIZE: f32 = DEFAULT_FONT_SIZE_POINTS * DEFAULT_FONT_DPI / 72.0;
const DEFAULT_BASELINE_RATIO: f32 = 0.73;
const DEFAULT_GLYPH_COVERAGE_BOOST: f32 = 0.2;

const BLACK: u32 = 0x0c0c0c;
const WHITE: u32 = 0xd8d8d8;
const CURSOR: u32 = 0xb8bb26;
const GHOSTTY_CONFIG_PATH: &str = "~/.config/ghostty/config";
const MIN_CELL_WIDTH: usize = 4;
const MIN_CELL_HEIGHT: usize = 8;

#[derive(Debug, Clone)]
pub(super) struct TerminalConfig {
    pub(super) font_family: Option<String>,
    pub(super) font_path: Option<PathBuf>,
    pub(super) font_size: f32,
    pub(super) font_pixels: f32,
    pub(super) metrics: TerminalMetrics,
    pub(super) hinting: Hinting,
    pub(super) shaping: Shaping,
    /// Alpha curve tweak for glyph antialiasing; 0 disables.
    pub(super) glyph_coverage_boost: f32,
    pub(super) theme: TerminalTheme,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TerminalMetrics {
    pub(super) cell_width: usize,
    pub(super) cell_height: usize,
    pub(super) baseline: isize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TerminalTheme {
    pub(super) background: u32,
    pub(super) foreground: u32,
    pub(super) cursor: u32,
    pub(super) palette: [u32; 256],
}

impl TerminalConfig {
    pub(super) fn load() -> Self {
        let mut config = Self::default();
        let config_path = expand_path(GHOSTTY_CONFIG_PATH);

        if let Err(error) = config.load_file(&config_path, 0) {
            debug!(path = %config_path.display(), %error, "failed to load Ghostty config");
        }

        config.finalize_metrics();
        config
    }

    fn load_file(&mut self, path: &Path, depth: usize) -> Result<()> {
        if depth > 8 {
            return Ok(());
        }

        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        let base_dir = path.parent().unwrap_or_else(|| Path::new("/"));

        for line in contents.lines() {
            let Some((key, value)) = ghostty_config_entry(line) else {
                continue;
            };

            if key == "config-file" {
                let include_path = ghostty_config_path(value, base_dir);
                if let Err(error) = self.load_file(&include_path, depth + 1) {
                    debug!(path = %include_path.display(), %error, "failed to load included Ghostty config");
                }
                continue;
            }

            self.apply_entry(key, value);
        }

        Ok(())
    }

    pub(super) fn apply_entry(&mut self, key: &str, value: &str) {
        match key {
            "background" => {
                if let Some(color) = parse_hex_color(value) {
                    self.theme.background = color;
                    self.theme.palette[0] = color;
                }
            }
            "foreground" => {
                if let Some(color) = parse_hex_color(value) {
                    self.theme.foreground = color;
                    self.theme.palette[7] = color;
                }
            }
            "cursor-color" => {
                if let Some(color) = parse_hex_color(value) {
                    self.theme.cursor = color;
                }
            }
            "palette" => {
                if let Some((index, color)) = parse_palette_entry(value) {
                    self.theme.palette[index as usize] = color;
                }
            }
            "font-family" => self.font_family = Some(unquote(value).to_string()),
            "font-size" => {
                if let Ok(font_size) = value.parse::<f32>() {
                    self.font_size = font_size;
                }
            }
            _ => {}
        }
    }

    pub(super) fn finalize_metrics(&mut self) {
        self.finalize_metrics_for_dpi(DEFAULT_FONT_DPI);
    }

    pub(super) fn finalize_metrics_for_scale(&mut self, scale_factor: f64) {
        let scale_override = env_f64("VIA_FONT_SCALE");
        let effective_scale_factor = scale_override.unwrap_or(scale_factor);
        debug!(
            reported_scale_factor = scale_factor,
            scale_override, effective_scale_factor, "terminal font scale finalized"
        );
        self.finalize_metrics_for_dpi(DEFAULT_FONT_DPI * effective_scale_factor.max(0.5) as f32);
    }

    fn finalize_metrics_for_dpi(&mut self, dpi: f32) {
        let font_pixel_scale = env_f32("VIA_FONT_PIXEL_SCALE").unwrap_or(1.0).max(0.1);
        let cell_width_scale = env_f32("VIA_CELL_WIDTH_SCALE").unwrap_or(1.0).max(0.1);
        let cell_height_scale = env_f32("VIA_CELL_HEIGHT_SCALE").unwrap_or(1.0).max(0.1);
        let baseline_ratio = env_f32("VIA_BASELINE_RATIO")
            .unwrap_or(DEFAULT_BASELINE_RATIO)
            .clamp(0.0, 1.0);

        self.font_pixels = points_to_pixels(self.font_size, dpi) * font_pixel_scale;
        let scale = (self.font_pixels / DEFAULT_FONT_PIXEL_SIZE).max(0.5);
        self.metrics.cell_width = ((DEFAULT_CELL_WIDTH as f32 * scale * cell_width_scale).round()
            as usize)
            .max(MIN_CELL_WIDTH);
        self.metrics.cell_height =
            ((DEFAULT_CELL_HEIGHT as f32 * scale * cell_height_scale).round() as usize)
                .max(MIN_CELL_HEIGHT);
        self.metrics.baseline = (self.metrics.cell_height as f32 * baseline_ratio).round() as isize;

        self.hinting = env_hinting().unwrap_or(Hinting::Disabled);
        self.shaping = env_shaping().unwrap_or(Shaping::Advanced);
        self.glyph_coverage_boost = env_f32("VIA_FONT_COVERAGE_BOOST")
            .unwrap_or(DEFAULT_GLYPH_COVERAGE_BOOST)
            .clamp(0.0, 2.0);

        debug!(
            font_points = self.font_size,
            font_pixels = self.font_pixels,
            cell_width = self.metrics.cell_width,
            cell_height = self.metrics.cell_height,
            baseline = self.metrics.baseline,
            font_pixel_scale,
            cell_width_scale,
            cell_height_scale,
            baseline_ratio,
            ?self.hinting,
            ?self.shaping,
            glyph_coverage_boost = self.glyph_coverage_boost,
            "terminal font metrics finalized"
        );
    }
}

fn env_hinting() -> Option<Hinting> {
    match std::env::var("VIA_FONT_HINTING").ok()?.as_str() {
        "1" | "true" | "on" | "enabled" => Some(Hinting::Enabled),
        "0" | "false" | "off" | "disabled" => Some(Hinting::Disabled),
        _ => None,
    }
}

fn env_shaping() -> Option<Shaping> {
    match std::env::var("VIA_FONT_SHAPING").ok()?.as_str() {
        "advanced" => Some(Shaping::Advanced),
        "basic" => Some(Shaping::Basic),
        _ => None,
    }
}

pub(super) fn env_f32(key: &str) -> Option<f32> {
    let value = std::env::var(key).ok()?;
    let parsed = value.parse::<f32>().ok()?;
    parsed.is_finite().then_some(parsed)
}

pub(super) fn env_f64(key: &str) -> Option<f64> {
    let value = std::env::var(key).ok()?;
    let parsed = value.parse::<f64>().ok()?;
    parsed.is_finite().then_some(parsed)
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            font_family: None,
            font_path: std::env::var_os("VIA_FONT_PATH").map(PathBuf::from),
            font_size: DEFAULT_FONT_SIZE_POINTS,
            font_pixels: DEFAULT_FONT_PIXEL_SIZE,
            metrics: TerminalMetrics::default(),
            hinting: Hinting::Disabled,
            shaping: Shaping::Advanced,
            glyph_coverage_boost: DEFAULT_GLYPH_COVERAGE_BOOST,
            theme: TerminalTheme::default(),
        }
    }
}

impl Default for TerminalMetrics {
    fn default() -> Self {
        Self {
            cell_width: DEFAULT_CELL_WIDTH,
            cell_height: DEFAULT_CELL_HEIGHT,
            baseline: 16,
        }
    }
}

impl TerminalTheme {
    /// Generate a DSR 997 sequence that notifies a running terminal application
    /// that the terminal's color scheme (dark/light) has changed.
    ///
    /// opentui-based apps (OpenCode, etc.) enable Mode 2031 color-scheme
    /// updates at startup and listen for `CSI ? 997 ; {1|2} n` to re-query
    /// the terminal's foreground/background colors via OSC 10/11.  Sending
    /// this notification after applying a new theme makes those apps pick up
    /// the new colors without being restarted.
    pub(super) fn color_scheme_notification(&self) -> Vec<u8> {
        let mode = if is_light_background(self.background) {
            2 // light
        } else {
            1 // dark
        };
        format!("\x1b[?997;{mode}n").into_bytes()
    }
}

fn is_light_background(color: u32) -> bool {
    let r = (color >> 16) & 0xff;
    let g = (color >> 8) & 0xff;
    let b = color & 0xff;

    r * 299 + g * 587 + b * 114 > 127_500
}

impl Default for TerminalTheme {
    fn default() -> Self {
        let mut palette = default_ansi_256_palette();
        let defaults = [
            BLACK, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, WHITE, 0x928374,
            0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
        ];

        for (index, color) in defaults.into_iter().enumerate() {
            palette[index] = color;
        }

        Self {
            background: BLACK,
            foreground: WHITE,
            cursor: CURSOR,
            palette,
        }
    }
}

fn default_ansi_256_palette() -> [u32; 256] {
    let mut palette = [0u32; 256];
    let xterm_16 = [
        0x000000, 0x800000, 0x008000, 0x808000, 0x000080, 0x800080, 0x008080, 0xc0c0c0, 0x808080,
        0xff0000, 0x00ff00, 0xffff00, 0x0000ff, 0xff00ff, 0x00ffff, 0xffffff,
    ];
    for (index, color) in xterm_16.into_iter().enumerate() {
        palette[index] = color;
    }

    let levels = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
    let mut index = 16usize;
    for &r in &levels {
        for &g in &levels {
            for &b in &levels {
                palette[index] = ((r as u32) << 16) | ((g as u32) << 8) | b as u32;
                index += 1;
            }
        }
    }

    for gray in 0..24u32 {
        let value = 8 + gray * 10;
        palette[232 + gray as usize] = (value << 16) | (value << 8) | value;
    }

    palette
}

fn points_to_pixels(points: f32, dpi: f32) -> f32 {
    points * dpi / 72.0
}

pub(super) fn ghostty_config_entry(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();

    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    let (key, value) = line.split_once('=')?;
    let mut value = value.trim();

    if let Some(rest) = value.strip_prefix('?') {
        value = rest.trim();
    }

    Some((key.trim(), unquote(value)))
}

fn ghostty_config_path(value: &str, base_dir: &Path) -> PathBuf {
    let path = expand_path(value);

    if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    }
}

fn expand_path(path: &str) -> PathBuf {
    let path = unquote(path);

    if path == "~" {
        return home_dir();
    }

    if let Some(rest) = path.strip_prefix("~/") {
        return home_dir().join(rest);
    }

    PathBuf::from(path)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
}

fn parse_palette_entry(value: &str) -> Option<(u8, u32)> {
    let (index, color) = value.split_once('=')?;
    let index = index.trim().parse::<u8>().ok()?;
    let color = parse_hex_color(color.trim())?;

    Some((index, color))
}

fn parse_hex_color(value: &str) -> Option<u32> {
    let value = value.trim().trim_start_matches('#');

    if value.len() != 6 {
        return None;
    }

    u32::from_str_radix(value, 16).ok()
}

use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::unbounded;
use minifb::{Key, KeyRepeat, MouseButton, MouseMode, Window, WindowOptions};
use tokio::sync::mpsc::Receiver as TokioReceiver;
use tracing::{debug, info};

use crate::config::Config;
use crate::event::{Event, UiCommand, UiEvent};
use crate::mediator::EventSender;

mod config;
mod font;
mod input;
mod layout;
mod links;
mod pane;
mod render;

use config::{TerminalConfig, TerminalMetrics};
use font::FontRenderer;
use input::{TextInput, forward_mouse_scroll, forward_special_keys, forward_text_input, try_clipboard_paste};
use layout::{PaneLayoutMode, PaneSplitDirection, SplitLayout, handle_layout_shortcuts};
use pane::TerminalPane;

const INITIAL_WIDTH: usize = 960;
const INITIAL_HEIGHT: usize = 540;

pub struct GhosttyUi {
    config: Config,
    events: EventSender,
    ui_commands: TokioReceiver<UiCommand>,
    pending_agent_write: Option<PendingAgentWrite>,
}

struct PendingAgentWrite {
    ready_at: Instant,
    bytes: Vec<u8>,
}

impl GhosttyUi {
    pub fn new(config: Config, events: EventSender, ui_commands: TokioReceiver<UiCommand>) -> Self {
        Self {
            config,
            events,
            ui_commands,
            pending_agent_write: None,
        }
    }

    pub fn describe_backend(&self) {
        info!(
            "native window host selected; Ghostty surface integration boundary is in ui::ghostty"
        );
    }

    pub fn run(mut self) -> Result<()> {
        let mut window = Window::new(
            "Spectre",
            INITIAL_WIDTH,
            INITIAL_HEIGHT,
            WindowOptions {
                resize: true,
                ..WindowOptions::default()
            },
        )
        .context("failed to create native window")?;
        window.set_target_fps(60);

        let (input_tx, input_rx) = unbounded();
        let (paste_tx, paste_rx) = unbounded();
        window.set_input_callback(Box::new(TextInput::new(input_tx, paste_tx)));

        let terminal_config = TerminalConfig::load();
        let mut font_renderer = FontRenderer::new(&terminal_config)?;
        let mut buffer = vec![terminal_config.theme.background; INITIAL_WIDTH * INITIAL_HEIGHT];
        let mut panes =
            self.create_panes(INITIAL_WIDTH, INITIAL_HEIGHT, terminal_config.metrics)?;
        let mut active_pane = 0;
        let mut pane_layout_mode = PaneLayoutMode::Split;
        let mut pane_split_direction =
            PaneSplitDirection::for_window(INITIAL_WIDTH, INITIAL_HEIGHT);
        let mut layout = SplitLayout::for_window(
            INITIAL_WIDTH,
            INITIAL_HEIGHT,
            panes.len(),
            pane_layout_mode,
            pane_split_direction,
        );
        let nvim_args = nvim_args(&self.config);

        panes[0].spawn(
            &self.config.nvim_command,
            nvim_args,
            &self.config.working_directory,
        )?;

        info!(panes = panes.len(), "native terminal window ready");
        let mut left_mouse_down = false;
        let mut force_redraw = true;

        while window.is_open() {
            let mut frame_dirty = force_redraw;
            force_redraw = false;
            let (width, height) = window.get_size();
            frame_dirty |= ensure_buffer_size(
                &mut buffer,
                width,
                height,
                terminal_config.theme.background,
            );
            let pressed_keys = window.get_keys_pressed(KeyRepeat::Yes);
            let alt = window.is_key_down(Key::LeftAlt) || window.is_key_down(Key::RightAlt);
            let layout_shortcut_consumed = handle_layout_shortcuts(
                &pressed_keys,
                alt,
                panes.len(),
                &mut pane_layout_mode,
                &mut pane_split_direction,
                &mut active_pane,
            );
            frame_dirty |= layout_shortcut_consumed;

            let new_layout = SplitLayout::for_window(
                width,
                height,
                panes.len(),
                pane_layout_mode,
                pane_split_direction,
            );
            if new_layout != layout {
                layout = new_layout;
                frame_dirty = true;

                for (index, pane) in panes.iter_mut().enumerate() {
                    let rect = layout.pane(index);
                    if rect.width == 0 || rect.height == 0 {
                        continue;
                    }

                    if let Some(size) = pane.resize(rect.width, rect.height) {
                        debug!(pane = pane.title, ?size, "resized terminal pane");
                    }
                }
            }

            for pane in &mut panes {
                frame_dirty |= pane.drain_output();
            }
            frame_dirty |= self.forward_ui_commands(&mut panes)?;
            frame_dirty |= self.flush_pending_agent_write(&mut panes)?;
            frame_dirty |= forward_text_input(
                &input_rx,
                &window,
                &mut panes[active_pane],
                layout_shortcut_consumed,
            )?;
            frame_dirty |= try_clipboard_paste(
                &window,
                &paste_rx,
                &mut panes[active_pane],
                layout_shortcut_consumed,
            )?;
            frame_dirty |= forward_special_keys(
                &pressed_keys,
                &window,
                &mut panes[active_pane],
                layout_shortcut_consumed,
            )?;
            frame_dirty |= forward_mouse_scroll(
                &window,
                &layout,
                &mut panes,
                layout_shortcut_consumed,
            );
            frame_dirty |= self.forward_file_reference_click(
                &window,
                &panes,
                &layout,
                &mut active_pane,
                &mut left_mouse_down,
            );

            if !frame_dirty {
                window.update();
                std::thread::sleep(Duration::from_millis(8));
                continue;
            }

            buffer.fill(terminal_config.theme.background);
            for (index, pane) in panes.iter_mut().enumerate() {
                pane.draw(
                    &mut font_renderer,
                    &mut buffer,
                    width,
                    height,
                    layout.pane(index),
                    index == active_pane,
                );
            }
            window
                .update_with_buffer(&buffer, width, height)
                .context("failed to update native window")?;
        }

        Ok(())
    }

    fn create_panes(
        &self,
        width: usize,
        height: usize,
        metrics: TerminalMetrics,
    ) -> Result<Vec<TerminalPane>> {
        let layout = SplitLayout::for_window(
            width,
            height,
            self.pane_count(),
            PaneLayoutMode::Split,
            PaneSplitDirection::for_window(width, height),
        );
        let mut panes = vec![TerminalPane::new(
            "nvim",
            layout.pane(0).width,
            layout.pane(0).height,
            metrics,
        )?];

        if let Some(agent_command) = self.config.agent_command.as_deref() {
            let mut pane = TerminalPane::new(
                "agent",
                layout.pane(1).width,
                layout.pane(1).height,
                metrics,
            )?;
            pane.spawn_shell_command(agent_command, &self.config.working_directory)?;
            panes.push(pane);
        }

        Ok(panes)
    }

    fn pane_count(&self) -> usize {
        if self.config.agent_command.is_some() {
            2
        } else {
            1
        }
    }

    fn forward_file_reference_click(
        &self,
        window: &Window,
        panes: &[TerminalPane],
        layout: &SplitLayout,
        active_pane: &mut usize,
        left_mouse_down: &mut bool,
    ) -> bool {
        let is_down = window.get_mouse_down(MouseButton::Left);
        let just_pressed = is_down && !*left_mouse_down;
        *left_mouse_down = is_down;

        if !just_pressed {
            return false;
        }

        let Some((x, y)) = window.get_unscaled_mouse_pos(MouseMode::Clamp) else {
            return false;
        };

        let x = x as usize;
        let y = y as usize;
        let Some((pane_index, rect)) = layout.pane_at(x, y) else {
            return false;
        };
        let pane_changed = *active_pane != pane_index;
        *active_pane = pane_index;

        let metrics = panes[pane_index].metrics();
        let row = (y - rect.y) / metrics.cell_height;
        let column = (x - rect.x) / metrics.cell_width;
        let Some(target) =
            panes[pane_index].file_reference_at(row, column, &self.config.working_directory)
        else {
            return pane_changed;
        };

        info!(
            path = %target.path.display(),
            line = ?target.line,
            "file reference clicked"
        );
        self.events.try_send(Event::Ui(UiEvent::OpenRequested {
            path: target.path,
            line: target.line,
        }));

        true
    }

    fn forward_ui_commands(&mut self, panes: &mut [TerminalPane]) -> Result<bool> {
        let mut changed = false;

        while let Ok(command) = self.ui_commands.try_recv() {
            changed = true;
            match command {
                UiCommand::EditorContextChanged { path, line, column } => {
                    let Some(agent_pane) = panes.get_mut(1) else {
                        continue;
                    };
                    let update =
                        format_context_update(&path, line, column, &self.config.working_directory);

                    info!(path = %path.display(), line, column, "forwarding editor context to agent");
                    agent_pane.write_all(update.as_bytes())?;
                    self.pending_agent_write = None;
                }
                UiCommand::VisualSelectionChanged {
                    path,
                    start_line,
                    end_line,
                } => {
                    let Some(agent_pane) = panes.get_mut(1) else {
                        continue;
                    };
                    let update = format_selection_update(
                        &path,
                        start_line,
                        end_line,
                        &self.config.working_directory,
                    );

                    info!(path = %path.display(), start_line, end_line, "forwarding visual selection to agent");
                    agent_pane.write_all(b"\x15")?;
                    self.pending_agent_write = Some(PendingAgentWrite {
                        ready_at: Instant::now() + Duration::from_millis(30),
                        bytes: update.into_bytes(),
                    });
                }
            }
        }

        Ok(changed)
    }

    fn flush_pending_agent_write(&mut self, panes: &mut [TerminalPane]) -> Result<bool> {
        let Some(pending) = &self.pending_agent_write else {
            return Ok(false);
        };

        if Instant::now() < pending.ready_at {
            return Ok(false);
        }

        let Some(agent_pane) = panes.get_mut(1) else {
            self.pending_agent_write = None;
            return Ok(false);
        };
        let pending = self
            .pending_agent_write
            .take()
            .expect("pending write exists");

        agent_pane.write_all(&pending.bytes)?;
        Ok(true)
    }
}

fn nvim_args(config: &Config) -> Vec<OsString> {
    vec![
        OsString::from("--listen"),
        config.nvim_socket_path.clone().into_os_string(),
        OsString::from("--cmd"),
        OsString::from(format!(
            "lua vim.g.spectre_editor_socket = {}",
            lua_string_literal(&config.editor_socket_path)
        )),
        OsString::from("-c"),
        OsString::from(format!(
            "luafile {}",
            vim_fnameescape(&config.nvim_context_bridge_path)
        )),
    ]
}

fn format_context_update(
    path: &Path,
    _line: u32,
    _column: u32,
    working_directory: &Path,
) -> String {
    let display_path = path.strip_prefix(working_directory).unwrap_or(path);

    format!("@{}\n", display_path.display())
}

fn format_selection_update(
    path: &Path,
    start_line: u32,
    end_line: u32,
    working_directory: &Path,
) -> String {
    let display_path = path.strip_prefix(working_directory).unwrap_or(path);

    format!("@{}:{start_line}-{end_line}", display_path.display())
}

fn lua_string_literal(path: &Path) -> String {
    let mut quoted = String::from("\"");

    for ch in path.to_string_lossy().chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            _ => quoted.push(ch),
        }
    }

    quoted.push('"');
    quoted
}

fn vim_fnameescape(path: &Path) -> String {
    let mut escaped = String::new();

    for ch in path.to_string_lossy().chars() {
        match ch {
            '\\' | ' ' | '\t' | '\n' | '*' | '?' | '[' | '{' | '`' | '$' | '%' | '#' | '\''
            | '"' | '|' | '!' | '<' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }

    escaped
}

fn ensure_buffer_size(buffer: &mut Vec<u32>, width: usize, height: usize, fill: u32) -> bool {
    let len = width.saturating_mul(height);

    if buffer.len() != len {
        buffer.resize(len, fill);
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::pty::TerminalSize;
    use config::ghostty_config_entry;
    use layout::PaneRect;
    use links::{LinkSpan, file_reference_at, file_target_from_uri, parse_vt_hyperlinks};

    #[test]
    fn parses_ghostty_config_entries() {
        assert_eq!(
            ghostty_config_entry("config-file = ?\"~/.config/theme.conf\""),
            Some(("config-file", "~/.config/theme.conf"))
        );
        assert_eq!(
            ghostty_config_entry("font-family = \"JetBrainsMono Nerd Font\""),
            Some(("font-family", "JetBrainsMono Nerd Font"))
        );
    }

    #[test]
    fn parses_theme_colors() {
        let mut config = TerminalConfig::default();

        config.apply_entry("background", "#0f0d21");
        config.apply_entry("foreground", "#ffffff");
        config.apply_entry("palette", "4=#b0c3f8");

        assert_eq!(config.theme.background, 0x0f0d21);
        assert_eq!(config.theme.foreground, 0xffffff);
        assert_eq!(config.theme.palette[4], 0xb0c3f8);
    }

    #[test]
    fn scales_metrics_from_ghostty_font_size() {
        let mut config = TerminalConfig {
            font_size: 9.0,
            ..TerminalConfig::default()
        };

        config.finalize_metrics();

        assert_eq!(config.font_pixels, 12.0);
        assert_eq!(config.metrics.cell_width, 8);
        assert_eq!(config.metrics.cell_height, 17);
    }

    #[test]
    fn finds_reference_under_column() {
        let target = file_reference_at("open project.md:12 please", 7, Path::new("/repo")).unwrap();

        assert_eq!(target.path, PathBuf::from("/repo/project.md"));
        assert_eq!(target.line, Some(12));
    }

    #[test]
    fn trims_common_surrounding_punctuation() {
        let target = file_reference_at("see `src/main.rs:8`, ok", 6, Path::new("/repo")).unwrap();

        assert_eq!(target.path, PathBuf::from("/repo/src/main.rs"));
        assert_eq!(target.line, Some(8));
    }

    #[test]
    fn trims_trailing_colon_without_line_number() {
        let target =
            file_reference_at("see src/ui.rs: for details", 5, Path::new("/repo")).unwrap();

        assert_eq!(target.path, PathBuf::from("/repo/src/ui.rs"));
        assert_eq!(target.line, None);
    }

    #[test]
    fn ignores_plain_words() {
        assert!(file_reference_at("no file here", 1, Path::new("/repo")).is_none());
    }

    #[test]
    fn creates_single_pane_layout_without_agent() {
        let layout = SplitLayout::for_window(
            100,
            50,
            1,
            PaneLayoutMode::Split,
            PaneSplitDirection::Vertical,
        );

        assert_eq!(
            layout.pane(0),
            PaneRect {
                x: 0,
                y: 0,
                width: 100,
                height: 50,
            }
        );
    }

    #[test]
    fn creates_vertical_split_layout_for_agent() {
        let layout = SplitLayout::for_window(
            100,
            50,
            2,
            PaneLayoutMode::Split,
            PaneSplitDirection::Vertical,
        );

        assert_eq!(
            layout.pane(0),
            PaneRect {
                x: 0,
                y: 0,
                width: 49,
                height: 50,
            }
        );
        assert_eq!(
            layout.pane(1),
            PaneRect {
                x: 51,
                y: 0,
                width: 49,
                height: 50,
            }
        );
        assert_eq!(layout.pane_at(10, 10).map(|(index, _)| index), Some(0));
        assert_eq!(layout.pane_at(60, 10).map(|(index, _)| index), Some(1));
        assert_eq!(layout.pane_at(50, 10), None);
    }

    #[test]
    fn creates_horizontal_split_layout_for_agent() {
        let layout = SplitLayout::for_window(
            50,
            100,
            2,
            PaneLayoutMode::Split,
            PaneSplitDirection::Horizontal,
        );

        assert_eq!(
            layout.pane(0),
            PaneRect {
                x: 0,
                y: 0,
                width: 50,
                height: 49,
            }
        );
        assert_eq!(
            layout.pane(1),
            PaneRect {
                x: 0,
                y: 51,
                width: 50,
                height: 49,
            }
        );
        assert_eq!(layout.pane_at(10, 10).map(|(index, _)| index), Some(0));
        assert_eq!(layout.pane_at(10, 60).map(|(index, _)| index), Some(1));
        assert_eq!(layout.pane_at(10, 50), None);
    }

    #[test]
    fn selects_initial_split_direction_from_window_shape() {
        assert_eq!(
            PaneSplitDirection::for_window(50, 100),
            PaneSplitDirection::Horizontal
        );
        assert_eq!(
            PaneSplitDirection::for_window(100, 50),
            PaneSplitDirection::Vertical
        );
        assert_eq!(
            PaneSplitDirection::for_window(100, 100),
            PaneSplitDirection::Vertical
        );
    }

    #[test]
    fn creates_nvim_maximized_layout() {
        let layout = SplitLayout::for_window(
            100,
            50,
            2,
            PaneLayoutMode::NvimMaximized,
            PaneSplitDirection::Vertical,
        );

        assert_eq!(
            layout.pane(0),
            PaneRect {
                x: 0,
                y: 0,
                width: 100,
                height: 50,
            }
        );
        assert_eq!(
            layout.pane(1),
            PaneRect {
                x: 100,
                y: 0,
                width: 0,
                height: 0,
            }
        );
        assert_eq!(layout.pane_at(10, 10).map(|(index, _)| index), Some(0));
    }

    #[test]
    fn creates_agent_maximized_layout() {
        let layout = SplitLayout::for_window(
            100,
            50,
            2,
            PaneLayoutMode::AgentMaximized,
            PaneSplitDirection::Vertical,
        );

        assert_eq!(
            layout.pane(0),
            PaneRect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            }
        );
        assert_eq!(
            layout.pane(1),
            PaneRect {
                x: 0,
                y: 0,
                width: 100,
                height: 50,
            }
        );
        assert_eq!(layout.pane_at(10, 10).map(|(index, _)| index), Some(1));
    }

    #[test]
    fn maximized_layouts_keep_hidden_panes_zero_sized() {
        let nvim_layout = SplitLayout::for_window(
            100,
            50,
            2,
            PaneLayoutMode::NvimMaximized,
            PaneSplitDirection::Vertical,
        );
        let agent_layout = SplitLayout::for_window(
            100,
            50,
            2,
            PaneLayoutMode::AgentMaximized,
            PaneSplitDirection::Vertical,
        );

        assert_eq!(nvim_layout.pane(1).width, 0);
        assert_eq!(nvim_layout.pane(1).height, 0);
        assert_eq!(agent_layout.pane(0).width, 0);
        assert_eq!(agent_layout.pane(0).height, 0);
    }

    #[test]
    fn maps_alt_number_shortcuts_to_layout_modes() {
        let mut mode = PaneLayoutMode::Split;
        let mut split_direction = PaneSplitDirection::Vertical;
        let mut active_pane = 1;

        assert!(handle_layout_shortcuts(
            &[Key::Key1],
            true,
            2,
            &mut mode,
            &mut split_direction,
            &mut active_pane
        ));
        assert_eq!(mode, PaneLayoutMode::NvimMaximized);
        assert_eq!(active_pane, 0);
        assert!(handle_layout_shortcuts(
            &[Key::Key2],
            true,
            2,
            &mut mode,
            &mut split_direction,
            &mut active_pane
        ));
        assert_eq!(mode, PaneLayoutMode::Split);
        assert_eq!(active_pane, 0);
        assert!(handle_layout_shortcuts(
            &[Key::Key3],
            true,
            2,
            &mut mode,
            &mut split_direction,
            &mut active_pane
        ));
        assert_eq!(mode, PaneLayoutMode::AgentMaximized);
        assert_eq!(active_pane, 1);
    }

    #[test]
    fn maps_alt_arrow_shortcuts_to_active_panes() {
        let mut mode = PaneLayoutMode::Split;
        let mut split_direction = PaneSplitDirection::Vertical;
        let mut active_pane = 1;

        assert!(handle_layout_shortcuts(
            &[Key::Left],
            true,
            2,
            &mut mode,
            &mut split_direction,
            &mut active_pane
        ));
        assert_eq!(mode, PaneLayoutMode::Split);
        assert_eq!(active_pane, 0);
        assert!(handle_layout_shortcuts(
            &[Key::Right],
            true,
            2,
            &mut mode,
            &mut split_direction,
            &mut active_pane
        ));
        assert_eq!(mode, PaneLayoutMode::Split);
        assert_eq!(active_pane, 1);
    }

    #[test]
    fn maps_alt_j_shortcut_to_split_direction_toggle() {
        let mut mode = PaneLayoutMode::Split;
        let mut split_direction = PaneSplitDirection::Vertical;
        let mut active_pane = 0;

        assert!(handle_layout_shortcuts(
            &[Key::J],
            true,
            2,
            &mut mode,
            &mut split_direction,
            &mut active_pane
        ));
        assert_eq!(mode, PaneLayoutMode::Split);
        assert_eq!(split_direction, PaneSplitDirection::Horizontal);
        assert_eq!(active_pane, 0);
        assert!(handle_layout_shortcuts(
            &[Key::J],
            true,
            2,
            &mut mode,
            &mut split_direction,
            &mut active_pane
        ));
        assert_eq!(split_direction, PaneSplitDirection::Vertical);
    }

    #[test]
    fn nvim_args_install_context_bridge() {
        let config = Config {
            nvim_command: "nvim".to_string(),
            agent_command: None,
            nvim_socket_path: PathBuf::from("/tmp/spectre-nvim.sock"),
            editor_socket_path: PathBuf::from("/tmp/spectre-editor.sock"),
            nvim_context_bridge_path: PathBuf::from("/repo/nvim/context bridge.lua"),
            working_directory: PathBuf::from("/repo"),
        };
        let args = nvim_args(&config);

        assert_eq!(args[0], OsString::from("--listen"));
        assert_eq!(args[1], OsString::from("/tmp/spectre-nvim.sock"));
        assert_eq!(args[2], OsString::from("--cmd"));
        assert_eq!(
            args[3],
            OsString::from(r#"lua vim.g.spectre_editor_socket = "/tmp/spectre-editor.sock""#)
        );
        assert_eq!(args[4], OsString::from("-c"));
        assert_eq!(
            args[5],
            OsString::from("luafile /repo/nvim/context\\ bridge.lua")
        );
    }

    #[test]
    fn context_update_uses_relative_file_path() {
        assert_eq!(
            format_context_update(Path::new("/repo/src/main.rs"), 42, 7, Path::new("/repo")),
            "@src/main.rs\n"
        );
    }

    #[test]
    fn selection_update_uses_relative_file_path_and_line_range() {
        assert_eq!(
            format_selection_update(Path::new("/repo/src/main.rs"), 3, 8, Path::new("/repo")),
            "@src/main.rs:3-8"
        );
    }

    #[test]
    fn vim_fnameescape_escapes_command_path_characters() {
        assert_eq!(
            vim_fnameescape(Path::new("/repo/nvim/context bridge's.lua")),
            "/repo/nvim/context\\ bridge\\'s.lua"
        );
    }

    #[test]
    fn parses_osc8_hyperlink_spans() {
        let rows = parse_vt_hyperlinks(
            b"see \x1b]8;;file:///repo/src/main.rs:8\x1b\\main\x1b]8;;\x1b\\ now",
            test_terminal_size(),
        );

        assert_eq!(
            rows[0],
            vec![LinkSpan {
                start: 4,
                end: 8,
                uri: "file:///repo/src/main.rs:8".to_string(),
            }]
        );
    }

    #[test]
    fn parses_bel_terminated_osc8_hyperlink_spans() {
        let rows = parse_vt_hyperlinks(
            b"\x1b]8;;src/lib.rs:3\x07lib\x1b]8;;\x07",
            test_terminal_size(),
        );

        assert_eq!(
            rows[0],
            vec![LinkSpan {
                start: 0,
                end: 3,
                uri: "src/lib.rs:3".to_string(),
            }]
        );
    }

    #[test]
    fn tracks_osc8_links_after_cursor_movement() {
        let rows = parse_vt_hyperlinks(
            b"\x1b[2;5H\x1b]8;;file:///repo/src/main.rs:8\x1b\\main\x1b]8;;\x1b\\",
            test_terminal_size(),
        );

        assert_eq!(
            rows[1],
            vec![LinkSpan {
                start: 4,
                end: 8,
                uri: "file:///repo/src/main.rs:8".to_string(),
            }]
        );
    }

    #[test]
    fn converts_file_uri_to_target() {
        let target =
            file_target_from_uri("file:///repo/src/main%20file.rs:8", Path::new("/fallback"))
                .unwrap();

        assert_eq!(target.path, PathBuf::from("/repo/src/main file.rs"));
        assert_eq!(target.line, Some(8));
    }

    #[test]
    fn ignores_non_file_uris() {
        assert!(
            file_target_from_uri("https://example.com/src/main.rs", Path::new("/repo")).is_none()
        );
    }

    fn test_terminal_size() -> TerminalSize {
        TerminalSize {
            rows: 5,
            cols: 40,
            pixel_width: 400,
            pixel_height: 100,
        }
    }
}

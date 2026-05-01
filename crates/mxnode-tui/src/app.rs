//! App state machine + key event handling.
//!
//! The dashboard is a simple state machine: a list of node tabs, one
//! selected at a time, plus a few toggles (paused, show-help,
//! show-logs). Mouse + keyboard both manipulate the same state.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use mxnode_core::NodeIndex;
use tokio::sync::Mutex;

use crate::metrics::{LogLevel, NodeSnapshot};

/// One entry in the fleet tab list.
#[derive(Clone)]
pub struct NodeHandle {
    pub index: NodeIndex,
    pub label: String,
    pub unit: String,
    pub api_port: u16,
    pub workdir: std::path::PathBuf,
    pub snapshot: Arc<Mutex<NodeSnapshot>>,
}

#[derive(Clone)]
pub struct App {
    pub nodes: Vec<NodeHandle>,
    pub selected: usize,
    pub paused: bool,
    pub show_help: bool,
    pub show_logs: bool,
    /// When `true`, the top instance/chain/block/gauge panels collapse
    /// and the log panel takes the entire body. Toggled with `f`.
    pub focus_logs: bool,
    /// Last-pressed key (status bar nag for unrecognised keys).
    pub last_key_hint: Option<String>,
    /// Frozen snapshots captured when `paused` flipped on. The renderer
    /// reads from these instead of the live `Mutex<NodeSnapshot>`s, so
    /// every panel (logs, sparklines, chain info) genuinely freezes.
    /// `None` when not paused.
    pub frozen: Option<Vec<crate::metrics::NodeSnapshot>>,
    /// Minimum log level to render. Lines below this are hidden along
    /// with their continuation rows (table borders that inherited the
    /// parent line's classification). Default: `Info` — operators rarely
    /// want the full DEBUG/TRACE firehose without asking for it.
    pub log_min_level: LogLevel,
    /// Case-insensitive substring filter for log lines. `None` = no
    /// filter; `Some("")` is treated the same. Stored lowercase so the
    /// renderer doesn't re-lowercase every frame.
    pub log_text_filter: Option<String>,
    /// Filter prompt input mode — when `true`, key events go into
    /// `filter_buffer` instead of triggering shortcuts.
    pub editing_filter: bool,
    pub filter_buffer: String,
    /// Network environment label rendered as a coloured badge in the
    /// header (e.g. `mainnet`, `testnet`, `devnet`). `None` = no badge.
    pub environment: Option<String>,
    /// Brand string shown in the header. Defaults to `"mxnode"`.
    pub title: String,
}

impl App {
    pub fn new(nodes: Vec<NodeHandle>) -> Self {
        Self {
            nodes,
            selected: 0,
            paused: false,
            show_help: false,
            show_logs: true,
            focus_logs: false,
            last_key_hint: None,
            frozen: None,
            log_min_level: LogLevel::Info,
            log_text_filter: None,
            editing_filter: false,
            filter_buffer: String::new(),
            environment: None,
            // Mirrors `BrandingSection::default()` so a freshly-instantiated
            // `App` (e.g. in tests) renders the same banner as a real
            // production launch.
            title: "By XOXNO ✦ TrustStaking".to_string(),
        }
    }

    pub fn current(&self) -> Option<&NodeHandle> {
        self.nodes.get(self.selected)
    }

    pub fn next_node(&mut self) {
        if self.nodes.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.nodes.len();
    }

    pub fn prev_node(&mut self) {
        if self.nodes.is_empty() {
            return;
        }
        if self.selected == 0 {
            self.selected = self.nodes.len() - 1;
        } else {
            self.selected -= 1;
        }
    }

    pub fn select(&mut self, idx: usize) {
        if idx < self.nodes.len() {
            self.selected = idx;
        }
    }

    /// Handle one key event. Returns `true` when the operator wants the
    /// dashboard to exit.
    pub fn on_key(&mut self, k: KeyEvent) -> bool {
        // Ctrl+C always exits, even mid-filter-input.
        if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('c')) {
            return true;
        }

        // Filter prompt mode swallows all keys until Enter / Esc.
        if self.editing_filter {
            match k.code {
                KeyCode::Esc => {
                    self.editing_filter = false;
                    self.filter_buffer.clear();
                }
                KeyCode::Enter => {
                    self.editing_filter = false;
                    let trimmed = self.filter_buffer.trim();
                    self.log_text_filter = if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_lowercase())
                    };
                    self.filter_buffer.clear();
                }
                KeyCode::Backspace => {
                    self.filter_buffer.pop();
                }
                KeyCode::Char(c) => {
                    self.filter_buffer.push(c);
                }
                _ => {}
            }
            return false;
        }

        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Tab | KeyCode::Right => self.next_node(),
            KeyCode::BackTab | KeyCode::Left => self.prev_node(),
            KeyCode::Char('?') | KeyCode::Char('h') => self.show_help = !self.show_help,
            KeyCode::Char('l') => {
                self.show_logs = !self.show_logs;
                if !self.show_logs {
                    self.focus_logs = false;
                }
            }
            KeyCode::Char('f') | KeyCode::Char('F') => {
                self.focus_logs = !self.focus_logs;
                if self.focus_logs {
                    self.show_logs = true;
                }
            }
            KeyCode::Char('p') | KeyCode::Char(' ') => self.paused = !self.paused,
            // Log filtering ─────────────────────────────────────────
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.log_min_level = self.log_min_level.step_up();
            }
            KeyCode::Char('-') | KeyCode::Char('_') => {
                self.log_min_level = self.log_min_level.step_down();
            }
            KeyCode::Char('/') => {
                self.editing_filter = true;
                self.filter_buffer = self.log_text_filter.clone().unwrap_or_default();
            }
            KeyCode::Char('c') => {
                self.log_text_filter = None;
                self.log_min_level = LogLevel::Info;
            }
            // ────────────────────────────────────────────────────────
            KeyCode::Char(c) if c.is_ascii_digit() => {
                if let Some(d) = c.to_digit(10) {
                    if d > 0 {
                        self.select((d as usize).saturating_sub(1));
                    }
                }
            }
            other => {
                self.last_key_hint = Some(format!("unrecognised: {:?}", other));
            }
        }
        false
    }

    /// Handle one mouse event. Currently: scroll = nothing (logs aren't
    /// scrollable yet), click on a tab = select that node.
    pub fn on_mouse(&mut self, m: MouseEvent, tab_columns: &[(u16, u16)]) {
        if let MouseEventKind::Down(_) = m.kind {
            // Tab strip lives on row 1 (header bar = row 0). Compare
            // pointer X against each tab's column range.
            if m.row == 1 {
                for (i, (start, end)) in tab_columns.iter().enumerate() {
                    if m.column >= *start && m.column < *end {
                        self.select(i);
                        break;
                    }
                }
            }
        }
    }
}

//! Top-level renderer.
//!
//! Layout (terminal height ≥ 30, width ≥ 110):
//!
//! ```text
//!  row 0   :: header bar (clock, fleet count, paused marker)
//!  row 1   :: tab strip with health glyph per node
//!  row 2.. :: instance info (left ~58%) | block info (right ~42%)
//!  ...     :: chain info (left tall)    | gauge stack (cpu+mem, epoch, network)
//!  ...     :: log panel — tail of newest *.log file under <workdir>/logs/
//!  bottom  :: keybinding hints
//! ```
//!
//! In **focus mode** (`f`), the body is replaced entirely with the log
//! panel — useful for tailing logs at full height. Narrow terminals
//! (<110 cols) collapse the left/right split into a single column.
//!
//! The renderer is **synchronous**. The event loop in `lib.rs` clones
//! the per-node snapshot under a brief lock and hands it in here, so
//! `pause` can freeze the whole frame (logs included) by reusing the
//! captured clone instead of re-locking.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Gauge, Paragraph, Row, Sparkline, Table, Tabs, Wrap,
};
use ratatui::Frame;

use crate::app::App;
use crate::metrics::{LogLevel, LogLine, NodeSnapshot, SyncState};
use crate::theme;

pub struct DrawContext {
    /// Tab column ranges captured during the most recent draw, used by
    /// the mouse handler to map clicks to tab indices.
    pub tab_columns: Vec<(u16, u16)>,
}

/// Render one full frame.
///
/// `current` is the snapshot for the currently-selected node, or
/// `None` when no node is selected (empty fleet). The caller acquires
/// it under a brief mutex lock or — when `app.paused` — clones from
/// `app.frozen`. This keeps the renderer synchronous so we don't have
/// to bridge async/sync inside `terminal.draw`.
pub fn draw(
    frame: &mut Frame<'_>,
    app: &App,
    ctx: &mut DrawContext,
    current: Option<(&str, &NodeSnapshot)>,
) {
    let area = frame.area();
    let body_focus = app.focus_logs && app.show_logs && current.is_some();
    // Header is now a bordered box (3 rows: top border + content + bottom
    // border) to match the rest of the panels' design language. Tab strip
    // is 2 rows (tabs + thin separator).
    let constraints: Vec<Constraint> = if body_focus {
        vec![
            Constraint::Length(3), // header bordered
            Constraint::Length(2), // tabs + underline
            Constraint::Min(10),   // log panel
            Constraint::Length(1), // status bar
        ]
    } else if app.show_logs {
        vec![
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Min(20),
            Constraint::Min(15),
            Constraint::Length(1),
        ]
    } else {
        vec![
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Min(15),
            Constraint::Length(1),
        ]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    draw_header(frame, chunks[0], app);
    draw_tabs(frame, chunks[1], app, ctx);

    if body_focus {
        // chunks[2] is the log panel taking the entire body.
        if let Some((_, snap)) = current {
            draw_log_panel(frame, chunks[2], app, snap);
        }
        draw_status_bar(frame, chunks[3], app);
    } else if app.show_logs {
        // chunks[2] = body, chunks[3] = logs, chunks[4] = status
        if let Some((label, snap)) = current {
            draw_body(frame, chunks[2], label, snap);
            draw_log_panel(frame, chunks[3], app, snap);
        } else {
            draw_empty_state(frame, chunks[2]);
        }
        draw_status_bar(frame, chunks[4], app);
    } else {
        if let Some((label, snap)) = current {
            draw_body(frame, chunks[2], label, snap);
        } else {
            draw_empty_state(frame, chunks[2]);
        }
        draw_status_bar(frame, chunks[3], app);
    }

    if app.show_help {
        draw_help_overlay(frame, area);
    }
}

// ── Header ───────────────────────────────────────────────────────────
//
// Bordered box, single content row. Left side: brand · env badge ·
// node count · fleet health (with words, not just glyphs). Right
// side: mode badges (Focus, Paused) · clock. Groups separated by a
// dim `│` so the eye can chunk them.

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    // Outer bordered box matches the panel design language.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .style(theme::header_bar());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Left side ──────────────────────────────────────────────────────
    let mut left: Vec<Span> = Vec::new();
    // Vertical accent bar followed by the brand string. Whatever the
    // operator put in `[branding].title` renders verbatim (raw spans —
    // no extra "dashboard" suffix).
    left.push(Span::styled("▌ ", Style::default().fg(theme::ACCENT)));
    left.push(Span::styled(app.title.clone(), theme::brand()));

    if let Some(env) = app.environment.as_deref() {
        push_group_pipe(&mut left);
        let (dot_color, label_color) = env_palette(env);
        left.push(Span::styled("●", Style::default().fg(dot_color)));
        left.push(Span::raw(" "));
        left.push(Span::styled(
            env.to_string(),
            Style::default()
                .fg(label_color)
                .add_modifier(Modifier::BOLD),
        ));
    }

    push_group_pipe(&mut left);
    left.push(Span::styled(
        format!("{} ", app.nodes.len()),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    ));
    left.push(Span::styled(
        if app.nodes.len() == 1 {
            "node"
        } else {
            "nodes"
        },
        theme::label(),
    ));

    // Multikey squads manage many validator keys per observer
    // process. Sum across the fleet so the operator sees the headline
    // number at a glance — a 4-observer squad managing 800 keys reads
    // as `4 nodes · 800 keys` here. Hidden when no node reports
    // managed-keys (non-multikey deployments).
    let total_managed = fleet_managed_keys(app);
    if total_managed > 0 {
        push_group_pipe(&mut left);
        left.push(Span::styled(
            human_count(total_managed),
            Style::default()
                .fg(theme::HIGHLIGHT)
                .add_modifier(Modifier::BOLD),
        ));
        left.push(Span::raw(" "));
        left.push(Span::styled(
            if total_managed == 1 { "key" } else { "keys" },
            theme::label(),
        ));
    }

    // Fleet health — full-word labels so it's self-explanatory. Only
    // non-zero categories show, separated by a dim `·`. When the fleet
    // is fully healthy we collapse to a single `all synced` chip.
    let (ok, syncing, fail, starting) = fleet_health(app);
    let total = ok + syncing + fail + starting;
    if total > 0 {
        push_group_pipe(&mut left);
        if ok == total {
            left.push(Span::styled(
                "all synced",
                theme::ok().add_modifier(Modifier::BOLD),
            ));
        } else {
            let mut first = true;
            let mut push_part =
                |out: &mut Vec<Span>, n: usize, glyph: &str, word: &str, style: Style| {
                    if n == 0 {
                        return;
                    }
                    if !first {
                        out.push(Span::styled(" · ", theme::label()));
                    }
                    first = false;
                    out.push(Span::styled(format!("{n} "), style));
                    out.push(Span::styled(format!("{glyph} {word}"), style));
                };
            push_part(
                &mut left,
                ok,
                "✓",
                "synced",
                theme::ok().add_modifier(Modifier::BOLD),
            );
            push_part(
                &mut left,
                syncing,
                "↻",
                "syncing",
                theme::warn().add_modifier(Modifier::BOLD),
            );
            push_part(
                &mut left,
                fail,
                "✗",
                "down",
                theme::fail().add_modifier(Modifier::BOLD),
            );
            push_part(&mut left, starting, "…", "starting", theme::label());
        }
    }

    // Right side ─────────────────────────────────────────────────────
    let mut right: Vec<Span> = Vec::new();
    if app.focus_logs {
        right.push(Span::styled(
            "Focus: logs",
            theme::title().add_modifier(Modifier::BOLD),
        ));
        push_group_pipe(&mut right);
    }
    if app.paused {
        right.push(Span::styled(
            "⏸ Paused",
            theme::warn().add_modifier(Modifier::BOLD),
        ));
        push_group_pipe(&mut right);
    }
    // Wall-clock with millisecond resolution. Worth the extra three
    // characters now that the chain is moving block-time precision
    // from seconds to ms — operators correlating events on the
    // dashboard with the on-chain timestamp need the same scale.
    let clock = time::OffsetDateTime::now_utc()
        .format(time::macros::format_description!(
            "[hour]:[minute]:[second].[subsecond digits:3]Z"
        ))
        .unwrap_or_default();
    right.push(Span::styled("◷ ", theme::label()));
    right.push(Span::styled(clock, Style::default().fg(Color::White)));
    right.push(Span::raw(" "));

    // Split the inner row in two so the right-aligned paragraph
    // doesn't repaint over the left content. We give the right side a
    // budget proportional to its rendered width so the clock + mode
    // badges stay flush with the right edge regardless of how much
    // text the left side carries.
    let right_width: u16 = right
        .iter()
        .map(|s| s.content.chars().count() as u16)
        .sum::<u16>()
        .min(inner.width.saturating_sub(10));
    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(right_width + 1)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(left)).style(theme::header_bar()),
        halves[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(right).alignment(Alignment::Right)).style(theme::header_bar()),
        halves[1],
    );
}

/// Inserts ` │ ` (dim pipe) between header groups so the eye can
/// chunk them. Centralised so all separators stay visually identical.
fn push_group_pipe(out: &mut Vec<Span<'static>>) {
    out.push(Span::styled("  │  ", theme::label()));
}

/// Pick a (dot, label) colour pair for the environment badge so the
/// risk profile is glanceable: red-ish for mainnet, yellow for testnet,
/// cyan for devnet, neutral for unknown.
fn env_palette(env: &str) -> (Color, Color) {
    match env {
        "mainnet" => (theme::FAIL, Color::Rgb(255, 130, 130)),
        "testnet" => (theme::WARN, Color::Rgb(255, 220, 100)),
        "devnet" => (theme::ACCENT, Color::Rgb(120, 220, 240)),
        _ => (theme::MUTED, Color::White),
    }
}

/// Headline managed-key total for the fleet header. In a multikey
/// squad every observer loads the same `allValidatorsKeys.pem`, so each
/// reports the squad-wide count and a naive sum multiplies by node
/// count (four observers × 50 keys → 200). When `app.shares_keys` is
/// set we collapse to the max instead. Reads via `try_lock` so a poller
/// mid-write doesn't stall the header.
fn fleet_managed_keys(app: &App) -> u64 {
    let counts = app.nodes.iter().map(|h| {
        h.snapshot
            .try_lock()
            .ok()
            .and_then(|s| s.managed_keys_count)
            .unwrap_or(0)
    });
    if app.shares_keys {
        counts.max().unwrap_or(0)
    } else {
        counts.sum()
    }
}

/// Walk the fleet and count nodes per health state for the header
/// summary. Reads via `try_lock` so a poller mid-write doesn't stall
/// the header redraw — worst case we miss a count for one frame.
fn fleet_health(app: &App) -> (usize, usize, usize, usize) {
    let mut ok = 0;
    let mut syncing = 0;
    let mut fail = 0;
    let mut starting = 0;
    for h in &app.nodes {
        let Ok(snap) = h.snapshot.try_lock() else {
            starting += 1;
            continue;
        };
        match &snap.state {
            Some(SyncState::Synced { .. }) => ok += 1,
            Some(SyncState::BlockSync { .. }) | Some(SyncState::TrieSync { .. }) => syncing += 1,
            Some(SyncState::Unreachable) => fail += 1,
            Some(SyncState::Starting) | None => starting += 1,
        }
    }
    (ok, syncing, fail, starting)
}

// ── Tabs ─────────────────────────────────────────────────────────────

fn draw_tabs(frame: &mut Frame, area: Rect, app: &App, ctx: &mut DrawContext) {
    if app.nodes.is_empty() {
        frame.render_widget(
            Paragraph::new(" no nodes in state.toml — run `mxnode adopt` ").style(theme::dim()),
            area,
        );
        ctx.tab_columns.clear();
        return;
    }
    let titles: Vec<Line> = app
        .nodes
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let glyph = match h.snapshot.try_lock() {
                Ok(lock) => glyph_for(&lock),
                Err(_) => "?",
            };
            Line::from(vec![
                Span::styled(format!("{} ", i + 1), theme::dim()),
                Span::styled(
                    h.label.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(glyph, glyph_style(glyph)),
            ])
        })
        .collect();

    ctx.tab_columns.clear();
    let mut cursor: u16 = area.x + 1;
    for h in &app.nodes {
        let glyph = h.snapshot.try_lock().map(|s| glyph_for(&s)).unwrap_or("?");
        let label_width = 2 + h.label.chars().count() as u16 + 1 + glyph.chars().count() as u16 + 2;
        ctx.tab_columns.push((cursor, cursor + label_width));
        cursor += label_width + 1;
    }

    // Drop the "Nodes" title — the operator already knows what tabs
    // are, and the header bar shows the fleet count. The block keeps
    // a thin bottom border so the tab strip visually separates from
    // the body without burning a third row on a label.
    let tabs = Tabs::new(titles)
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_type(BorderType::Plain)
                .border_style(theme::border()),
        )
        .select(app.selected)
        .highlight_style(
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )
        .divider(Span::styled(" │ ", theme::dim()));
    frame.render_widget(tabs, area);
}

fn glyph_for(snap: &NodeSnapshot) -> &'static str {
    match &snap.state {
        Some(SyncState::Synced { .. }) => "✓",
        Some(SyncState::BlockSync { .. }) => "↻",
        Some(SyncState::TrieSync { .. }) => "↯",
        Some(SyncState::Starting) => "…",
        Some(SyncState::Unreachable) => "✗",
        None => "·",
    }
}

fn glyph_style(glyph: &str) -> Style {
    match glyph {
        "✓" => theme::ok().add_modifier(Modifier::BOLD),
        "↻" | "↯" => theme::warn().add_modifier(Modifier::BOLD),
        "✗" => theme::fail().add_modifier(Modifier::BOLD),
        "…" => theme::dim(),
        _ => Style::default(),
    }
}

// ── Body ─────────────────────────────────────────────────────────────

fn draw_body(frame: &mut Frame, area: Rect, label: &str, snap: &NodeSnapshot) {
    let narrow = area.width < 110;
    if narrow {
        let stacked = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(8),
                Constraint::Min(8),
                Constraint::Min(7),
                Constraint::Length(3),
                Constraint::Length(3),
            ])
            .split(area);
        draw_instance(frame, stacked[0], label, snap);
        draw_chain(frame, stacked[1], snap);
        draw_block_info(frame, stacked[2], snap);
        draw_load_row(frame, stacked[3], snap);
        // Epoch-cumulative bytes are folded into draw_network_row's
        // gauge titles, so no dedicated row is needed here.
        draw_network_row(frame, stacked[4], snap);
        return;
    }

    let halves = Layout::default()
        .direction(Direction::Horizontal)
        // Equal halves so the right column has enough room for the
        // longer gauge titles ("Rx X (X%) peak Y · Z this epoch")
        // without truncation. The left column had 58% historically
        // because the Chain row was the widest line in the panel —
        // it still fits comfortably at 50% on terminals ≥ 110 cols
        // (the threshold for the wide layout).
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(9), Constraint::Min(8)])
        .split(halves[0]);
    draw_instance(frame, left[0], label, snap);
    draw_chain(frame, left[1], snap);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(9),    // block info
            Constraint::Length(3), // cpu+mem row
            Constraint::Length(3), // epoch progress (or trie-sync gauge)
            Constraint::Length(3), // network rx+tx (with epoch totals folded in)
        ])
        .split(halves[1]);
    draw_block_info(frame, right[0], snap);
    draw_load_row(frame, right[1], snap);
    // While the node is in trie sync the epoch hasn't started — the
    // Epoch gauge would just show "0 rounds". Swap in a Trie-sync
    // gauge for that slot so the operator sees actual progress.
    if matches!(snap.state, Some(SyncState::TrieSync { .. })) {
        draw_trie_sync_row(frame, right[2], snap);
    } else {
        draw_epoch_row(frame, right[2], snap);
    }
    draw_network_row(frame, right[3], snap);
}

// ── Title / value helpers ────────────────────────────────────────────

fn box_title(label: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::raw(" "),
        Span::styled(label, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled("info", theme::title()),
        Span::raw(": "),
    ])
}

fn bordered(title: Line<'static>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .title(title)
}

fn lbl(text: &str) -> Span<'_> {
    Span::styled(text, theme::label())
}

fn val<'a>(text: impl Into<String>) -> Span<'a> {
    Span::styled(text.into(), Style::default().fg(Color::White))
}

fn val_strong<'a>(text: impl Into<String>) -> Span<'a> {
    Span::styled(
        text.into(),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )
}

// ── Instance panel ───────────────────────────────────────────────────

fn draw_instance(frame: &mut Frame, area: Rect, label: &str, snap: &NodeSnapshot) {
    let m = &snap.metrics;
    let shard = match m.get_u64("erd_shard_id") {
        Some(s) if s == u32::MAX as u64 => "metachain".to_string(),
        Some(n) => format!("shard {n}"),
        None => "shard ?".to_string(),
    };
    let node_type = m.get_str("erd_node_type").unwrap_or("?");
    let peer_type = m.get_str("erd_peer_type").unwrap_or("");
    let nt_display = if peer_type.is_empty() || peer_type == "observer" {
        node_type.to_string()
    } else {
        format!("{node_type} - {peer_type}")
    };
    let app_version = m.get_str("erd_app_version").unwrap_or("?");
    let pubkey = m.get_str("erd_public_key_block_sign").unwrap_or("");
    let pubkey_short = if pubkey.len() > 14 {
        format!("{}…{}", &pubkey[..8], &pubkey[pubkey.len() - 6..])
    } else {
        pubkey.to_string()
    };
    let signed = m.get_u64("erd_count_consensus").unwrap_or(0);
    let accepted = m
        .get_u64("erd_count_consensus_accepted_blocks")
        .unwrap_or(0);
    let proposed = m.get_u64("erd_count_leader").unwrap_or(0);
    let proposed_acc = m.get_u64("erd_count_accepted_blocks").unwrap_or(0);
    // Validator + Proposer counters are inherently zero on a node
    // whose peer_type isn't a validator (observers don't participate
    // in consensus). Hide both rows in that case so the panel doesn't
    // burn lines on permanently-zero data. We show them when peer_type
    // says "validator" AND when peer_type is unknown / empty (early
    // boot — better to show 0/0 briefly than to flicker the rows in).
    let is_validator_peer = peer_type.is_empty() || peer_type.contains("validator");
    let chain_id = m.get_str("erd_chain_id").unwrap_or("?");
    let redundancy_level = m.get_i64("erd_redundancy_level");
    let redundancy_main_active = m.get_str("erd_redundancy_is_main_active");
    let redundancy_text = format_redundancy(redundancy_level, redundancy_main_active);

    let mut rows = vec![
        Row::new(vec![
            Cell::from(lbl("Name")),
            Cell::from(Line::from(vec![
                Span::styled(
                    label.to_string(),
                    Style::default()
                        .fg(theme::HIGHLIGHT)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  ("),
                Span::styled(shard, theme::label()),
                Span::raw(" • "),
                Span::styled(nt_display, theme::label()),
                Span::raw(")"),
            ])),
        ]),
        Row::new(vec![
            Cell::from(lbl("App")),
            Cell::from(Line::from(Span::styled(
                app_version.to_string(),
                theme::ok(),
            ))),
        ]),
        Row::new(vec![
            Cell::from(lbl("PubKey")),
            Cell::from(val(pubkey_short)),
        ]),
    ];
    if is_validator_peer {
        rows.push(Row::new(vec![
            Cell::from(lbl("Validator")),
            Cell::from(Line::from(vec![
                val_strong(signed.to_string()),
                Span::styled(" Signed ", theme::dim()),
                Span::raw("/ "),
                val_strong(accepted.to_string()),
                Span::styled(" Accepted", theme::dim()),
            ])),
        ]));
        rows.push(Row::new(vec![
            Cell::from(lbl("Proposer")),
            Cell::from(Line::from(vec![
                val_strong(proposed.to_string()),
                Span::styled(" Proposed ", theme::dim()),
                Span::raw("/ "),
                val_strong(proposed_acc.to_string()),
                Span::styled(" Accepted", theme::dim()),
            ])),
        ]));
    }
    rows.push(Row::new(vec![
        Cell::from(lbl("Chain")),
        Cell::from(val(chain_id.to_string())),
    ]));
    rows.push(Row::new(vec![
        Cell::from(lbl("Redundancy")),
        Cell::from(val(redundancy_text)),
    ]));
    // Managed-keys row only shows when the node is actively in
    // multikey mode (count > 0). Newer mx-chain-go builds expose
    // `/node/managed-keys/count` on every node — a regular validator
    // with a single `.pem` and a plain observer both report `0`
    // because they don't load keys via the multikey handler. We hide
    // the row in those cases so operators see "Managed Keys" only
    // when there's an `allValidatorsKeys.pem` actually loaded
    // (multikey main, multikey backup with redundancy > 0, etc).
    // A None here (endpoint missing on older builds) also stays
    // hidden — same outcome.
    if let Some(n) = snap.managed_keys_count.filter(|c| *c > 0) {
        rows.push(Row::new(vec![
            Cell::from(lbl("Managed Keys")),
            Cell::from(Line::from(vec![
                Span::styled(
                    human_count(n),
                    Style::default()
                        .fg(theme::HIGHLIGHT)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(if n == 1 { " key" } else { " keys" }, theme::label()),
            ])),
        ]));
    }
    let t = Table::new(rows, [Constraint::Length(13), Constraint::Min(20)])
        .block(bordered(box_title("MultiversX instance")));
    frame.render_widget(t, area);
}

/// Match the Go termui's redundancy display:
///   < 0 → "Inactive"
///   = 0 → "Main machine"
///   > 0 → "Back-up #N (main active: <flag>)"
/// > `is_main_active` is `"true"` / `"false"` / `"N/A"` from the metric.
fn format_redundancy(level: Option<i64>, is_main_active: Option<&str>) -> String {
    match level {
        None => "—".to_string(),
        Some(l) if l < 0 => "Inactive".to_string(),
        Some(0) => "Main machine".to_string(),
        Some(n) => match is_main_active {
            Some(s) if !s.is_empty() && s != "N/A" => format!("Back-up #{n} (main active: {s})"),
            _ => format!("Back-up #{n}"),
        },
    }
}

// ── Chain panel ──────────────────────────────────────────────────────

fn draw_chain(frame: &mut Frame, area: Rect, snap: &NodeSnapshot) {
    let m = &snap.metrics;
    let nonce = m.get_u64("erd_nonce").unwrap_or(0);
    let probable = m.get_u64("erd_probable_highest_nonce").unwrap_or(nonce);
    let epoch = m.get_u64("erd_epoch_number").unwrap_or(0);
    let round = m.get_u64("erd_current_round").unwrap_or(0);
    let synced_round = m.get_u64("erd_synchronized_round").unwrap_or(0);
    let txpool = m.get_u64("erd_tx_pool_load").unwrap_or(0);
    // Wire names cross-checked against
    // mx-chain-go/common/constants.go (constants the node actually
    // exposes via /node/status). Earlier mxnode read camel-case-ish
    // shorthands that never appear in the real payload, which is why
    // the "Processed" / "Val" / block-info widgets stayed at zero.
    let tx_processed = m.get_u64("erd_num_transactions_processed").unwrap_or(0);
    let peers = m.get_u64("erd_num_connected_peers").unwrap_or(0);
    let validators = m.get_u64("erd_intra_shard_validator_nodes").unwrap_or(0);
    let nodes = m.get_u64("erd_connected_nodes").unwrap_or(0);
    let round_time = m.get_u64("erd_round_time").unwrap_or(0);
    let live_validators = m.get_u64("erd_live_validator_nodes").unwrap_or(0);

    let status_line = Line::from(match &snap.state {
        Some(SyncState::Synced { .. }) => vec![Span::styled(
            "Synchronized",
            Style::default()
                .fg(theme::OK)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )],
        Some(SyncState::BlockSync { nonce, target }) => vec![
            Span::styled("Block sync ", theme::warn().add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("{nonce} → {target} ({} behind)", target - nonce),
                theme::warn(),
            ),
        ],
        Some(SyncState::TrieSync { processed, .. }) => {
            let pct = snap.trie_sync_pct();
            let total = snap.trie_total_nodes;
            let body = match (pct, total) {
                (Some(p), Some(t)) => {
                    format!("Trie sync — {} / {} nodes (~{}%)", processed, t, p)
                }
                (Some(p), None) => format!("Trie sync — {} nodes (~{}%)", processed, p),
                (None, Some(t)) => format!("Trie sync — {} / {} nodes", processed, t),
                (None, None) => format!("Trie sync — {} nodes processed", processed),
            };
            vec![Span::styled(
                body,
                theme::warn().add_modifier(Modifier::BOLD),
            )]
        }
        Some(SyncState::Starting) => vec![Span::styled(
            "Node is starting",
            theme::dim().add_modifier(Modifier::BOLD),
        )],
        Some(SyncState::Unreachable) => vec![Span::styled(
            "REST unreachable",
            theme::fail().add_modifier(Modifier::BOLD),
        )],
        None => vec![Span::styled("…", theme::dim())],
    });

    let rows = vec![
        Row::new(vec![Cell::from(lbl("Status")), Cell::from(status_line)]),
        Row::new(vec![
            Cell::from(lbl("Epoch")),
            Cell::from(val_strong(epoch.to_string())),
        ]),
        Row::new(vec![
            Cell::from(lbl("Nonce")),
            Cell::from(Line::from(vec![
                val_strong(nonce.to_string()),
                Span::styled(" / ", theme::dim()),
                val(probable.to_string()),
            ])),
        ]),
        Row::new(vec![
            Cell::from(lbl("Round")),
            Cell::from(Line::from(vec![
                val_strong(synced_round.to_string()),
                Span::styled(" / ", theme::dim()),
                val(round.to_string()),
                Span::styled(format!("  {round_time}s"), theme::dim()),
            ])),
        ]),
        Row::new(vec![
            Cell::from(lbl("TxPool")),
            Cell::from(Line::from(vec![
                val_strong(txpool.to_string()),
                Span::styled(" Pending ", theme::dim()),
                Span::raw("/ "),
                val(tx_processed.to_string()),
                Span::styled(" Processed", theme::dim()),
            ])),
        ]),
        Row::new(vec![
            Cell::from(lbl("Peers")),
            Cell::from(Line::from(vec![
                val_strong(peers.to_string()),
                Span::styled(" Intra ", theme::dim()),
                Span::raw("/ "),
                val(validators.to_string()),
                Span::styled(" Val ", theme::dim()),
                Span::raw("/ "),
                val(nodes.to_string()),
                Span::styled(" Nodes", theme::dim()),
            ])),
        ]),
        Row::new(vec![
            Cell::from(lbl("KnownVal")),
            Cell::from(val(live_validators.to_string())),
        ]),
    ];
    let t = Table::new(rows, [Constraint::Length(11), Constraint::Min(20)])
        .block(bordered(box_title("Chain")));
    frame.render_widget(t, area);
}

// ── Block info ───────────────────────────────────────────────────────

fn draw_block_info(frame: &mut Frame, area: Rect, snap: &NodeSnapshot) {
    let m = &snap.metrics;
    let nonce = m.get_u64("erd_nonce").unwrap_or(0);
    // The node does not expose a single `erd_block_size`; termui
    // computes the displayed size as header-size + miniblocks-size,
    // see mx-chain-go/cmd/termui/presenter/blockInfoGetters.go::GetBlockSize.
    let header_size = m.get_u64("erd_current_block_size").unwrap_or(0);
    let mini_blocks_size = m.get_u64("erd_mini_blocks_size").unwrap_or(0);
    let size = header_size.saturating_add(mini_blocks_size);
    let tx_in_block = m.get_u64("erd_num_tx_block").unwrap_or(0);
    let mb = m.get_u64("erd_num_mini_blocks").unwrap_or(0);
    let hash = m.get_str("erd_current_block_hash").unwrap_or("");
    let cross = m.get_str("erd_cross_check_block_height").unwrap_or("");
    let cstate = m.get_str("erd_consensus_state").unwrap_or("");
    let consensus_round_state = m.get_str("erd_consensus_round_state").unwrap_or("");
    let final_nonce = m.get_u64("erd_highest_final_nonce");
    let round_ts = m.get_u64("erd_current_round_timestamp");

    let hash_short = if hash.len() > 18 {
        format!("{}…{}", &hash[..10], &hash[hash.len() - 8..])
    } else {
        hash.to_string()
    };

    let mut rows = vec![
        Row::new(vec![
            Cell::from(lbl("Height")),
            Cell::from(Line::from(vec![
                val_strong(nonce.to_string()),
                Span::styled(format!("  {} Bytes", size), theme::dim()),
            ])),
        ]),
        Row::new(vec![
            Cell::from(lbl("Txs")),
            Cell::from(val_strong(tx_in_block.to_string())),
        ]),
        Row::new(vec![
            Cell::from(lbl("MiniBlocks")),
            Cell::from(val_strong(mb.to_string())),
        ]),
        Row::new(vec![Cell::from(lbl("Hash")), Cell::from(val(hash_short))]),
        Row::new(vec![
            Cell::from(lbl("Cross")),
            Cell::from(val(cross.to_string())),
        ]),
    ];
    if let Some(fn_) = final_nonce {
        rows.push(Row::new(vec![
            Cell::from(lbl("Final")),
            Cell::from(val_strong(fn_.to_string())),
        ]));
    }
    rows.push(Row::new(vec![
        Cell::from(lbl("Consensus")),
        Cell::from(Line::from(vec![
            val(cstate.to_string()),
            Span::styled(format!("  {}", consensus_round_state), theme::dim()),
        ])),
    ]));
    if let Some(ts) = round_ts {
        rows.push(Row::new(vec![
            Cell::from(lbl("Timestamp")),
            Cell::from(Line::from(vec![
                val_strong(format_unix_ts(ts)),
                Span::styled(format!("  ({})", ts), theme::dim()),
            ])),
        ]));
    }

    let t = Table::new(rows, [Constraint::Length(11), Constraint::Min(20)])
        .block(bordered(box_title("Block")));
    frame.render_widget(t, area);
}

/// Convert a unix timestamp emitted by the node — either seconds
/// (legacy) or milliseconds (post-Supernova ms block-time) — into a
/// formatted `YYYY-MM-DD HH:MM:SS.mmmZ` string.
///
/// We auto-detect the unit by magnitude: anything `> 9_999_999_999`
/// is treated as milliseconds. That threshold lands in year 2286 if
/// interpreted as seconds (we'll be long upgraded by then) and in
/// year 1973 if interpreted as milliseconds (we'll never see legit
/// chain timestamps from before 1973). The detector is therefore
/// unambiguous for any realistic on-chain value.
fn format_unix_ts(raw: u64) -> String {
    let nanos: i128 = if raw > 9_999_999_999 {
        // Treat as milliseconds; convert to nanoseconds.
        (raw as i128) * 1_000_000
    } else {
        // Treat as seconds; convert to nanoseconds.
        (raw as i128) * 1_000_000_000
    };
    let dt = match time::OffsetDateTime::from_unix_timestamp_nanos(nanos) {
        Ok(d) => d,
        Err(_) => return raw.to_string(),
    };
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    dt.format(&fmt).unwrap_or_else(|_| raw.to_string())
}

#[cfg(test)]
mod ts_tests {
    use super::format_unix_ts;

    #[test]
    fn formats_seconds_unix_timestamp_with_zero_ms() {
        // 1777158600 = 2026-04-25 23:10:00 UTC.
        let s = format_unix_ts(1_777_158_600);
        assert_eq!(s, "2026-04-25 23:10:00.000Z");
    }

    #[test]
    fn formats_millisecond_unix_timestamp_with_resolved_ms() {
        // Same instant, but in ms: ...600 sec → ...600_250 ms.
        let s = format_unix_ts(1_777_158_600_250);
        assert_eq!(s, "2026-04-25 23:10:00.250Z");
    }

    #[test]
    fn detector_threshold_is_around_year_2286_in_seconds() {
        // Below the threshold → seconds.
        let s = format_unix_ts(9_999_999_999);
        assert!(s.starts_with("2286-"), "got {s}");
        // Just above → milliseconds (year 1970-01-12).
        let ms = format_unix_ts(10_000_000_000);
        assert!(ms.starts_with("1970-"), "got {ms}");
    }
}

#[cfg(test)]
mod fleet_managed_keys_tests {
    use super::fleet_managed_keys;
    use crate::app::{App, NodeHandle};
    use crate::metrics::NodeSnapshot;
    use mxnode_core::NodeIndex;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn handle(idx: u16, count: Option<u64>) -> NodeHandle {
        let snap = NodeSnapshot {
            managed_keys_count: count,
            ..NodeSnapshot::default()
        };
        NodeHandle {
            index: NodeIndex::new(idx),
            label: format!("node-{idx}"),
            unit: format!("elrond-node-{idx}.service"),
            api_port: 8080 + idx,
            workdir: PathBuf::from(format!("/tmp/node-{idx}")),
            snapshot: Arc::new(Mutex::new(snap)),
        }
    }

    #[test]
    fn sums_managed_keys_across_distinct_validator_nodes() {
        // Four single-key validators each owning their own .pem —
        // distinct keys, sum is correct.
        let mut app = App::new(vec![
            handle(0, Some(1)),
            handle(1, Some(1)),
            handle(2, Some(1)),
            handle(3, Some(1)),
        ]);
        app.shares_keys = false;
        assert_eq!(fleet_managed_keys(&app), 4);
    }

    #[test]
    fn collapses_managed_keys_when_squad_shares_keys() {
        // Multikey squad: four observers loading the same
        // allValidatorsKeys.pem (50 keys). Header should read 50, not 200.
        let mut app = App::new(vec![
            handle(0, Some(50)),
            handle(1, Some(50)),
            handle(2, Some(50)),
            handle(3, Some(50)),
        ]);
        app.shares_keys = true;
        assert_eq!(fleet_managed_keys(&app), 50);
    }

    #[test]
    fn collapses_to_max_when_some_observers_have_not_loaded_yet() {
        // Mid-rollout: one observer hasn't reported yet (None → 0). Max
        // captures the squad's intended count, sum would understate it.
        let mut app = App::new(vec![
            handle(0, Some(50)),
            handle(1, Some(50)),
            handle(2, None),
            handle(3, Some(50)),
        ]);
        app.shares_keys = true;
        assert_eq!(fleet_managed_keys(&app), 50);
    }
}

// ── CPU + Memory ─────────────────────────────────────────────────────

fn draw_load_row(frame: &mut Frame, area: Rect, snap: &NodeSnapshot) {
    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let cpu = snap.metrics.get_u64("erd_cpu_load_percent").unwrap_or(0);
    let mem = snap.metrics.get_u64("erd_mem_load_percent").unwrap_or(0);
    let mem_total = snap.metrics.get_u64("erd_mem_total").unwrap_or(0);
    let mem_used = snap.metrics.get_u64("erd_mem_used_golang").unwrap_or(0);
    let mem_label = format!(
        "{}%  {} / {}",
        mem,
        human_bytes(mem_used),
        human_bytes(mem_total)
    );

    frame.render_widget(
        Gauge::default()
            .block(bordered(box_title("CPU")))
            .gauge_style(gauge_color(cpu))
            .percent(cpu.min(100) as u16)
            .label(format!("{}%", cpu)),
        halves[0],
    );
    frame.render_widget(
        Gauge::default()
            .block(bordered(box_title("Memory")))
            .gauge_style(gauge_color(mem))
            .percent(mem.min(100) as u16)
            .label(mem_label),
        halves[1],
    );
}

// ── Network rx + tx (with peaks, mirrors Go termui's per-host gauge) ─

fn draw_network_row(frame: &mut Frame, area: Rect, snap: &NodeSnapshot) {
    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let rx = snap.netin_hist.last().unwrap_or(0);
    let tx = snap.netout_hist.last().unwrap_or(0);
    let rx_peak = snap
        .metrics
        .get_u64("erd_network_recv_bps_peak")
        .unwrap_or(0);
    let tx_peak = snap
        .metrics
        .get_u64("erd_network_sent_bps_peak")
        .unwrap_or(0);
    let rx_pct = snap
        .metrics
        .get_u64("erd_network_recv_percent")
        .unwrap_or(0);
    let tx_pct = snap
        .metrics
        .get_u64("erd_network_sent_percent")
        .unwrap_or(0);
    // Cumulative bytes for the current epoch, folded into the gauge
    // titles so the operator sees current rate / peak / epoch total
    // in one place. mx-chain-go exposes them under the `_per_host`
    // suffix; the un-suffixed names mxnode used historically do not
    // exist in `/node/status`. Source: common/constants.go:191/194.
    let recv_epoch = snap
        .metrics
        .get_u64("erd_network_recv_bytes_in_epoch_per_host")
        .unwrap_or(0);
    let sent_epoch = snap
        .metrics
        .get_u64("erd_network_sent_bytes_in_epoch_per_host")
        .unwrap_or(0);
    let rx_data = snap.netin_hist.as_vec();
    let tx_data = snap.netout_hist.as_vec();
    render_net_gauge(
        frame,
        halves[0],
        "Rx",
        theme::ok(),
        rx,
        rx_pct,
        rx_peak,
        recv_epoch,
        &rx_data,
    );
    render_net_gauge(
        frame,
        halves[1],
        "Tx",
        theme::warn(),
        tx,
        tx_pct,
        tx_peak,
        sent_epoch,
        &tx_data,
    );
}

/// Render one Rx or Tx gauge with a width-aware title.
///
/// The title is built progressively: we always include the label + the
/// current rate, then add `(X%)`, `peak Y/s`, and the cumulative epoch
/// total only if there's room. The cumulative bytes get one of three
/// shapes depending on what fits:
///
///   - `· <Z> this epoch` — full form, ≥ ~14 chars of room
///   - `· <Z> epoch`      — medium form, ≥ ~9 chars
///   - `· <Z>`            — minimal form, ≥ ~3 chars
///
/// At very narrow widths the suffix is dropped entirely. Inter-segment
/// spacing is a single space (was double historically) so the title
/// reads compactly on every supported terminal width without losing
/// information.
#[allow(clippy::too_many_arguments)]
fn render_net_gauge(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    color: Style,
    rate: u64,
    pct: u64,
    peak: u64,
    epoch_total: u64,
    sparkline_data: &[u64],
) {
    // The gauge's `bordered` block consumes 2 columns (left + right
    // border). One leading + one trailing space inside the title keep
    // it from butting against the corners.
    let usable = (area.width as usize).saturating_sub(4);

    let label_str = format!("{label} ");
    let rate_str = format!("{}/s", human_bytes(rate));
    let pct_seg = format!(" ({}%)", pct);
    let peak_seg = format!(" peak {}/s", human_bytes(peak));
    let epoch_long = format!(" · {} this epoch", human_bytes(epoch_total));
    let epoch_med = format!(" · {} epoch", human_bytes(epoch_total));
    let epoch_short = format!(" · {}", human_bytes(epoch_total));

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(6);
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        label_str.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(rate_str.clone(), color));
    let mut used = label_str.len() + rate_str.len();

    let try_push = |seg: String, used: &mut usize, spans: &mut Vec<Span<'static>>| -> bool {
        if *used + seg.len() <= usable {
            *used += seg.len();
            spans.push(Span::styled(seg, theme::dim()));
            true
        } else {
            false
        }
    };

    // Drop priorities, in order of "what to ditch first when we run
    // out of room": peak (most expendable since the sparkline already
    // shows the silhouette), then `(X%)` saturation, then the epoch
    // total. The cumulative epoch number in particular is the most
    // unique-to-the-title datum (no other widget shows it now), so we
    // protect it with a three-tier fallback before giving up.
    let _ = try_push(pct_seg, &mut used, &mut spans);
    let _ = try_push(peak_seg, &mut used, &mut spans);
    let _ = try_push(epoch_long, &mut used, &mut spans)
        || try_push(epoch_med, &mut used, &mut spans)
        || try_push(epoch_short, &mut used, &mut spans);

    spans.push(Span::raw(" "));

    frame.render_widget(
        Sparkline::default()
            .block(bordered(Line::from(spans)))
            .style(color)
            .data(sparkline_data),
        area,
    );
}

// ── Trie sync progress (replaces Epoch slot during trie sync) ────────

fn draw_trie_sync_row(frame: &mut Frame, area: Rect, snap: &NodeSnapshot) {
    let (processed, _pct) = match &snap.state {
        Some(SyncState::TrieSync { processed, pct }) => (*processed, *pct),
        _ => (0, None),
    };
    let pct = snap.trie_sync_pct();
    let total = snap.trie_total_nodes;
    let label = match (pct, total) {
        (Some(p), Some(t)) => format!(
            "{} / {} nodes  ({}%)",
            human_count(processed),
            human_count(t),
            p
        ),
        (Some(p), None) => format!("{} nodes  ({}%)", human_count(processed), p),
        (None, Some(t)) => format!("{} / {} nodes", human_count(processed), human_count(t)),
        (None, None) => format!(
            "{} nodes processed (gateway unreachable)",
            human_count(processed)
        ),
    };
    frame.render_widget(
        Gauge::default()
            .block(bordered(box_title("Trie sync")))
            .gauge_style(theme::accent_gauge())
            .percent(pct.unwrap_or(0).min(100) as u16)
            .label(label),
        area,
    );
}

/// Human-formatted integer with thousands separators. The trie node
/// counts are easily 7-digit so the un-grouped form is hard to scan.
fn human_count(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

// ── Epoch progress ───────────────────────────────────────────────────

fn draw_epoch_row(frame: &mut Frame, area: Rect, snap: &NodeSnapshot) {
    let m = &snap.metrics;
    let cur_round = m.get_u64("erd_current_round").unwrap_or(0);
    let start_round = m.get_u64("erd_round_at_epoch_start").unwrap_or(0);
    let rounds_per_epoch = m.get_u64("erd_rounds_per_epoch").unwrap_or(0);
    let round_time_secs = m.get_u64("erd_round_time").unwrap_or(0);
    let progressed = cur_round.saturating_sub(start_round);
    let pct = if rounds_per_epoch > 0 {
        (progressed * 100 / rounds_per_epoch).min(100)
    } else {
        0
    };
    // Estimate epoch time-remaining the same way Go termui does:
    // rounds_remaining * round_time_seconds.
    let remaining_rounds = rounds_per_epoch.saturating_sub(progressed);
    let remaining = if round_time_secs > 0 {
        format_duration(remaining_rounds.saturating_mul(round_time_secs))
    } else {
        String::new()
    };
    let label = if rounds_per_epoch > 0 {
        if remaining.is_empty() {
            format!("{} / {} rounds  ({}%)", progressed, rounds_per_epoch, pct)
        } else {
            format!(
                "{} / {} rounds  ({}%)  ~{} remaining",
                progressed, rounds_per_epoch, pct, remaining
            )
        }
    } else {
        format!("{} rounds", progressed)
    };
    frame.render_widget(
        Gauge::default()
            .block(bordered(box_title("Epoch")))
            .gauge_style(theme::accent_gauge())
            .percent(pct as u16)
            .label(label),
        area,
    );
}

fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

fn gauge_color(pct: u64) -> Style {
    match pct {
        0..=60 => theme::ok(),
        61..=85 => theme::warn(),
        _ => theme::fail(),
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = n as f64;
    let mut idx = 0;
    while size >= 1024.0 && idx < UNITS.len() - 1 {
        size /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{n}{}", UNITS[idx])
    } else {
        format!("{:.1}{}", size, UNITS[idx])
    }
}

// ── Log panel ────────────────────────────────────────────────────────

fn draw_log_panel(frame: &mut Frame, area: Rect, app: &App, snap: &NodeSnapshot) {
    let path_hint = app
        .current()
        .map(|h| format!(" — {}/logs/", h.workdir.display()))
        .unwrap_or_default();
    let mut title_spans = vec![
        Span::raw(" "),
        Span::styled("Log", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled("info", theme::title()),
        Span::raw(": "),
        Span::styled(path_hint, theme::dim()),
        Span::raw(" "),
        Span::styled(format!("[≥{}]", app.log_min_level.label()), theme::title()),
    ];
    if let Some(filter) = &app.log_text_filter {
        title_spans.push(Span::raw(" "));
        title_spans.push(Span::styled(
            format!("[/{}]", filter),
            Style::default().fg(theme::HIGHLIGHT),
        ));
    }
    if app.paused {
        title_spans.push(Span::styled(
            "  ⏸ frozen",
            theme::warn().add_modifier(Modifier::BOLD),
        ));
    }
    let block = bordered(Line::from(title_spans));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // When the operator is editing the filter prompt, reserve the
    // bottom row of the inner panel for the input line.
    let (logs_area, prompt_area) = if app.editing_filter {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);
        (split[0], Some(split[1]))
    } else {
        (inner, None)
    };

    // Build (line, effective-level) pairs across the full buffer so
    // continuation rows inherit their parent's classification BEFORE
    // we filter — that way filtering an INFO parent also drops its
    // table-border children.
    let mut last_classified: LogLevel = LogLevel::Info;
    let mut all_effective: Vec<LogLevel> = Vec::with_capacity(snap.log_lines.len());
    for line in snap.log_lines.iter() {
        match line.level {
            LogLevel::Other => all_effective.push(last_classified),
            other => {
                last_classified = other;
                all_effective.push(other);
            }
        }
    }

    // Apply filters. Iterate from the newest end so we collect the
    // most recent `max_lines` matching rows without scanning the
    // entire buffer when filters are loose.
    let max_lines = logs_area.height as usize;
    let max_width = logs_area.width as usize;
    let min_severity = app.log_min_level.severity();
    let text_filter = app.log_text_filter.as_deref();
    let mut visible: Vec<(&LogLine, LogLevel)> = Vec::with_capacity(max_lines);
    for (line, eff) in snap.log_lines.iter().zip(all_effective.iter()).rev() {
        if eff.severity() < min_severity {
            continue;
        }
        if let Some(filter) = text_filter {
            if !line.raw.to_lowercase().contains(filter) {
                continue;
            }
        }
        visible.push((line, *eff));
        if visible.len() == max_lines {
            break;
        }
    }
    visible.reverse();

    let rendered: Vec<Line> = visible
        .iter()
        .map(|(l, eff)| render_log_line(l, *eff, max_width))
        .collect();
    frame.render_widget(
        Paragraph::new(rendered).wrap(Wrap { trim: false }),
        logs_area,
    );

    if let Some(area) = prompt_area {
        let prompt = Line::from(vec![
            Span::styled(" / ", theme::title().add_modifier(Modifier::BOLD)),
            Span::styled(app.filter_buffer.clone(), Style::default().fg(Color::White)),
            Span::styled("█", Style::default().fg(theme::ACCENT)),
            Span::styled(
                "    Enter to apply · Esc to cancel · Backspace to edit",
                theme::dim(),
            ),
        ]);
        frame.render_widget(Paragraph::new(prompt).style(theme::status_bar()), area);
    }
}

fn render_log_line(line: &LogLine, effective: LogLevel, max_width: usize) -> Line<'_> {
    let style = match effective {
        LogLevel::Error => theme::fail().add_modifier(Modifier::BOLD),
        LogLevel::Warn => theme::warn().add_modifier(Modifier::BOLD),
        LogLevel::Info => theme::log_info(),
        LogLevel::Debug => theme::log_debug(),
        LogLevel::Trace => theme::log_trace(),
        LogLevel::Other => theme::log_other(),
    };
    let mut text = line.raw.clone();
    if text.len() > max_width.saturating_mul(3).max(400) {
        text.truncate(max_width.saturating_mul(3).max(400));
        text.push_str(" …");
    }
    Line::from(Span::styled(text, style))
}

// ── Status bar ───────────────────────────────────────────────────────

fn draw_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans: Vec<Span> = Vec::with_capacity(64);
    spans.push(Span::raw(" "));

    // ── Group 1: navigation (no state) ──────────────────────────
    push_chip(&mut spans, "q", "quit", ChipState::Static);
    push_sep(&mut spans);
    push_chip(&mut spans, "↹/←→", "node", ChipState::Static);
    push_sep(&mut spans);
    push_chip(&mut spans, "1-9", "jump", ChipState::Static);

    push_group_sep(&mut spans);

    // ── Group 2: layout toggles (ON-state chips) ────────────────
    push_chip(&mut spans, "l", "logs", ChipState::Toggle(app.show_logs));
    push_sep(&mut spans);
    push_chip(&mut spans, "f", "focus", ChipState::Toggle(app.focus_logs));
    push_sep(&mut spans);
    push_chip(&mut spans, "p", "pause", ChipState::Toggle(app.paused));

    push_group_sep(&mut spans);

    // ── Group 3: log filters (with live values) ─────────────────
    let level_chip = level_chip(app.log_min_level);
    spans.extend(level_chip);
    push_sep(&mut spans);
    let filter_chip = filter_chip(app);
    spans.extend(filter_chip);
    let filters_active = app.log_min_level != LogLevel::Info || app.log_text_filter.is_some();
    if filters_active {
        push_sep(&mut spans);
        push_chip(&mut spans, "c", "clear", ChipState::Action);
    }

    push_group_sep(&mut spans);

    // ── Group 4: help (always last, plain) ──────────────────────
    push_chip(&mut spans, "?", "help", ChipState::Static);

    if let Some(hint) = &app.last_key_hint {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(format!("({hint})"), theme::warn()));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(theme::status_bar()),
        area,
    );
}

#[derive(Copy, Clone)]
enum ChipState {
    /// Plain chip — no on/off concept (q quit, ?, navigation).
    Static,
    /// Highlighted chip indicating an actionable single-shot key
    /// (e.g. `c clear` only shown when filters are active).
    Action,
    /// Toggle chip — bright pill style when on, dim when off.
    Toggle(bool),
}

fn push_chip<'a>(out: &mut Vec<Span<'a>>, key: &'a str, label: &'a str, state: ChipState) {
    let (key_style, label_style, prefix, suffix) = match state {
        ChipState::Static => (
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
            theme::dim(),
            "",
            "",
        ),
        ChipState::Action => (
            Style::default()
                .fg(theme::HIGHLIGHT)
                .add_modifier(Modifier::BOLD),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            "",
            "",
        ),
        ChipState::Toggle(true) => (
            Style::default()
                .fg(theme::OK)
                .bg(theme::CHIP_ON_BG)
                .add_modifier(Modifier::BOLD),
            Style::default()
                .fg(Color::White)
                .bg(theme::CHIP_ON_BG)
                .add_modifier(Modifier::BOLD),
            // Half-block left/right caps make the pill read as a chip
            // even on terminals that don't render true backgrounds.
            "▐",
            "▌",
        ),
        ChipState::Toggle(false) => (
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
            theme::dim(),
            "",
            "",
        ),
    };
    if !prefix.is_empty() {
        out.push(Span::styled(
            prefix.to_string(),
            Style::default().fg(theme::CHIP_ON_BG),
        ));
    }
    out.push(Span::styled(key.to_string(), key_style));
    out.push(Span::styled(format!(" {label}"), label_style));
    if !suffix.is_empty() {
        out.push(Span::styled(
            suffix.to_string(),
            Style::default().fg(theme::CHIP_ON_BG),
        ));
    }
}

fn push_sep(out: &mut Vec<Span<'static>>) {
    out.push(Span::styled(" · ", theme::dim()));
}

fn push_group_sep(out: &mut Vec<Span<'static>>) {
    out.push(Span::styled("  ┃  ", Style::default().fg(theme::BORDER)));
}

/// Level chip shows the live threshold in the level's own colour.
/// `+`/`-` keys appear before the value for discoverability.
fn level_chip(level: LogLevel) -> Vec<Span<'static>> {
    let value_style = match level {
        LogLevel::Error => theme::fail().add_modifier(Modifier::BOLD),
        LogLevel::Warn => theme::warn().add_modifier(Modifier::BOLD),
        LogLevel::Info => Style::default()
            .fg(theme::LOG_INFO)
            .add_modifier(Modifier::BOLD),
        LogLevel::Debug => Style::default()
            .fg(theme::LOG_DEBUG)
            .add_modifier(Modifier::BOLD),
        LogLevel::Trace => Style::default()
            .fg(theme::LOG_TRACE)
            .add_modifier(Modifier::BOLD),
        LogLevel::Other => theme::dim(),
    };
    vec![
        Span::styled(
            "+/-",
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" level ", theme::dim()),
        Span::styled("≥", theme::dim()),
        Span::styled(level.label().to_string(), value_style),
    ]
}

/// Filter chip shows the active substring (or `…` placeholder when
/// none). When the operator is editing the filter prompt, shows
/// `editing` to remind them where their keystrokes are going.
fn filter_chip(app: &App) -> Vec<Span<'static>> {
    let key = Span::styled(
        "/",
        Style::default()
            .fg(theme::ACCENT)
            .add_modifier(Modifier::BOLD),
    );
    let label = Span::styled(" filter ", theme::dim());
    let value: Span = if app.editing_filter {
        Span::styled(
            format!("\"{}_\"", app.filter_buffer),
            Style::default()
                .fg(theme::WARN)
                .bg(theme::CHIP_ON_BG)
                .add_modifier(Modifier::BOLD),
        )
    } else if let Some(f) = app.log_text_filter.as_deref() {
        let display = if f.len() > 18 { &f[..18] } else { f };
        Span::styled(
            format!("\"{}\"", display),
            Style::default()
                .fg(theme::HIGHLIGHT)
                .bg(theme::CHIP_ON_BG)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("none", theme::dim())
    };
    vec![key, label, value]
}

fn draw_empty_state(frame: &mut Frame, area: Rect) {
    let p = Paragraph::new(
        "no nodes installed.\n\nrun `mxnode install` (or `mxnode adopt` on an existing host).",
    )
    .alignment(Alignment::Center)
    .style(theme::dim());
    frame.render_widget(p, area);
}

fn draw_help_overlay(frame: &mut Frame, area: Rect) {
    let overlay = centered_rect(60, 70, area);
    let body = Paragraph::new(
        "mxnode dashboard — keybindings\n\n\
         Navigation\n\
           q, esc, ctrl+c   quit\n\
           tab / →          next node\n\
           shift+tab / ←    previous node\n\
           1-9              jump to node by index\n\
           click            tab → select node\n\n\
         Layout\n\
           l                toggle log panel\n\
           f                focus mode (logs full-screen)\n\
           p / space        pause (freezes panels + logs)\n\n\
         Log filtering\n\
           +, =             raise min level (TRACE → DEBUG → INFO → WARN → ERROR)\n\
           -, _             lower min level (more verbose)\n\
           /                text filter — type substring, Enter to apply, Esc to cancel\n\
           c                clear filters (back to INFO+, no text)\n\n\
         Other\n\
           ?, h             toggle this help\n",
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .title(Span::styled(" Help — ? to dismiss ", theme::title())),
    )
    .style(Style::default().bg(Color::Rgb(20, 20, 28)).fg(Color::White));
    frame.render_widget(Clear, overlay);
    frame.render_widget(body, overlay);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

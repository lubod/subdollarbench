//! `subdollarbench tui`: an htop-like, read-only view of the campaign API.
//!
//! The API is the source of truth: this module keeps a plain [`App`] state,
//! renders it with ratatui, and never derives a second ranking or ticket
//! status. Actions are deliberately out of scope (D2).

use anyhow::{Context, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table};
use ratatui::{Frame, Terminal};
use serde_json::Value;
use std::collections::BTreeSet;
use std::io::{stdout, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const LOG_MAX: usize = 2_000;
pub(crate) static RESTORE_CALLS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum View {
    #[default]
    Cells,
    Tickets,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Focus {
    #[default]
    Cells,
    Live,
    Tickets,
}

/// The TUI's whole mutable state. Keys and API pages update it; rendering
/// only reads it.
#[derive(Debug, Default)]
pub struct App {
    status: Value,
    campaigns: Vec<Value>,
    selected: usize,
    view: View,
    focus: Focus,
    tickets: Vec<Value>,
    selected_ticket: usize,
    log: Vec<(String, String)>,
    log_offset: u64,
    log_kinds: BTreeSet<String>,
    follow: bool,
    help: bool,
    engine_error: Option<String>,
}

impl App {
    pub fn new() -> Self {
        let mut log_kinds = BTreeSet::new();
        log_kinds.insert("verdict".to_string());
        log_kinds.insert("agent".to_string());
        Self {
            selected: 0,
            view: View::Cells,
            focus: Focus::Cells,
            log_kinds,
            follow: true,
            ..Self::default()
        }
    }

    pub fn set_status(&mut self, status: Value) {
        self.status = status;
        self.engine_error = None;
    }
    pub fn unreachable(&mut self, err: impl Into<String>) {
        self.engine_error = Some(err.into());
    }
    pub fn set_campaigns(&mut self, campaigns: Vec<Value>) {
        let previous = self.selected_id().map(str::to_string);
        self.campaigns = campaigns;
        if let Some(id) = previous {
            if let Some(pos) = self
                .campaigns
                .iter()
                .position(|c| c["id"].as_str() == Some(&id))
            {
                self.selected = pos;
                return;
            }
            self.selected = self.campaigns.len().saturating_sub(1);
            self.clear_cell_state();
            return;
        }
        self.selected = self.selected.min(self.campaigns.len().saturating_sub(1));
    }
    pub fn set_tickets(&mut self, tickets: Vec<Value>) {
        self.tickets = tickets;
        self.selected_ticket = 0;
        self.view = View::Tickets;
        self.focus = Focus::Tickets;
    }
    pub fn selected_cell(&self) -> Option<&Value> {
        self.campaigns.get(self.selected)
    }
    pub fn selected_id(&self) -> Option<&str> {
        self.selected_cell().and_then(|c| c["id"].as_str())
    }
    #[cfg(test)]
    pub fn campaigns(&self) -> &[Value] {
        &self.campaigns
    }
    #[cfg(test)]
    pub fn help_open(&self) -> bool {
        self.help
    }
    #[cfg(test)]
    pub fn log_kinds(&self) -> &BTreeSet<String> {
        &self.log_kinds
    }
    pub fn log_offset(&self) -> u64 {
        self.log_offset
    }
    #[cfg(test)]
    pub fn log_len(&self) -> usize {
        self.log.len()
    }

    #[cfg(test)]
    pub fn tickets_len(&self) -> usize {
        self.tickets.len()
    }

    pub fn apply_log_page(&mut self, offset: u64, lines: Vec<Value>) -> bool {
        if offset < self.log_offset {
            self.log.clear();
        }
        if lines.is_empty() {
            self.log_offset = offset;
            return false;
        }
        for l in lines {
            let kind = l["kind"].as_str().unwrap_or("agent").to_string();
            let text = l["text"]
                .as_str()
                .or_else(|| l["line"].as_str())
                .unwrap_or("")
                .to_string();
            self.log.push((kind, text));
        }
        if self.log.len() > LOG_MAX {
            let cut = self.log.len() - LOG_MAX;
            self.log.drain(0..cut);
        }
        self.log_offset = offset;
        true
    }

    pub fn toggle_kind(&mut self, kind: &str) -> bool {
        if !matches!(kind, "verdict" | "agent" | "tools" | "setup" | "telemetry") {
            return false;
        }
        if !self.log_kinds.remove(kind) {
            self.log_kinds.insert(kind.to_string());
        }
        self.log.clear();
        self.log_offset = 0;
        true
    }

    /// Returns true when the app should quit.
    pub fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
            return true;
        }
        if self.help {
            if code == KeyCode::Esc || code == KeyCode::Char('?') || code == KeyCode::Char('q') {
                self.help = false;
                return false;
            }
            return false;
        }
        match code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Up => {
                if self.view == View::Cells && self.selected > 0 {
                    self.selected -= 1;
                    self.clear_cell_state();
                } else if self.view == View::Tickets && self.selected_ticket > 0 {
                    self.selected_ticket -= 1;
                }
            }
            KeyCode::Down => {
                if self.view == View::Cells && self.selected + 1 < self.campaigns.len() {
                    self.selected += 1;
                    self.clear_cell_state();
                } else if self.view == View::Tickets
                    && self.selected_ticket + 1 < self.tickets.len()
                {
                    self.selected_ticket += 1;
                }
            }
            KeyCode::Enter => {
                if self.view == View::Cells {
                    self.view = View::Tickets;
                    self.focus = Focus::Tickets;
                }
            }
            KeyCode::Esc => {
                self.view = View::Cells;
                self.focus = Focus::Cells;
                self.follow = true;
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Cells => Focus::Live,
                    Focus::Live => Focus::Cells,
                    Focus::Tickets => Focus::Tickets,
                };
            }
            KeyCode::Char('v') => {
                self.toggle_kind("verdict");
            }
            KeyCode::Char('a') => {
                self.toggle_kind("agent");
            }
            KeyCode::Char('t') => {
                self.toggle_kind("tools");
            }
            KeyCode::Char('s') => {
                self.toggle_kind("setup");
            }
            KeyCode::End => self.follow = true,
            _ => {}
        }
        false
    }

    fn clear_cell_state(&mut self) {
        self.log.clear();
        self.log_offset = 0;
        self.tickets.clear();
        self.selected_ticket = 0;
    }

    fn filtered_log(&self) -> Vec<Line<'static>> {
        self.log
            .iter()
            .filter(|(k, _)| self.log_kinds.contains(k))
            .map(|(k, t)| {
                let style = if k == "verdict" && (t.contains("FAIL") || t.contains("ERR")) {
                    Style::default().fg(Color::Red)
                } else if k == "verdict" && (t.contains("PASS") || t.contains(" OK")) {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default()
                };
                Line::from(Span::styled(t.clone(), style))
            })
            .collect()
    }

    fn log_request_path(&self, after: u64) -> Option<String> {
        let id = self.selected_id()?;
        let kinds = self.log_kinds.iter().cloned().collect::<Vec<_>>().join(",");
        Some(if after == 0 {
            format!("/api/campaigns/{id}/log?tail=400&kinds={kinds}")
        } else {
            format!("/api/campaigns/{id}/log?after={after}&kinds={kinds}")
        })
    }
}

fn state_text(c: &Value) -> String {
    let state = c["state"].as_str().unwrap_or("?");
    let live = c.get("live").unwrap_or(&Value::Null);
    match state {
        "running" => format!(
            "run t{} {}/{}",
            live["ticket"]
                .as_u64()
                .or_else(|| c["current_stage"].as_u64())
                .unwrap_or(0),
            live["turn"].as_u64().unwrap_or(0),
            live["max_turns"].as_u64().unwrap_or(0)
        ),
        "stalled" => "stalled".to_string(),
        "done" => {
            let stop = c["stop_reason"].as_str().unwrap_or("");
            if stop.is_empty() {
                "done".into()
            } else {
                format!("done · {stop}")
            }
        }
        x => x.to_string(),
    }
}

fn short_model(c: &Value) -> String {
    let m = c["rollup"]["model"]
        .as_str()
        .or_else(|| c["model"].as_str())
        .unwrap_or("?");
    m.rsplit('/').next().unwrap_or(m).chars().take(28).collect()
}

fn points(c: &Value) -> f64 {
    c["rollup"]["career"]["points"]
        .as_f64()
        .or_else(|| c["rollup"]["points"].as_f64())
        .unwrap_or(0.0)
}
fn cost(c: &Value) -> f64 {
    c["rollup"]["career"]["total_cost_usd"]
        .as_f64()
        .or_else(|| c["rollup"]["total_cost_usd"].as_f64())
        .unwrap_or(0.0)
}
/// A career phase (not a standalone cell): the rollup has `phase`, or an
/// explicit career block. Its denominator is the 76-point ladder.
fn is_career(c: &Value) -> bool {
    c["rollup"]["phase"].is_number() || c["rollup"]["career"].is_object()
}

fn max_points(c: &Value) -> f64 {
    if is_career(c) {
        76.0
    } else {
        c["rollup"]["max_points"].as_f64().unwrap_or(76.0)
    }
}

/// R55 F1: a running career phase counts the chain's points so far plus
/// this phase's own. A finished phase's `career` block already holds the
/// total.
fn career_points(c: &Value, all: &[Value]) -> f64 {
    if let Some(p) = c["rollup"]["career"]["points"].as_f64() {
        return p;
    }
    let own = c["rollup"]["points"].as_f64().unwrap_or(0.0);
    let chain = c["rollup"]["chain_from"]
        .as_str()
        .or_else(|| c["chain_from"].as_str());
    if let Some(id) = chain {
        if let Some(prev) = all.iter().find(|p| p["id"].as_str() == Some(id)) {
            return career_points(prev, all) + own;
        }
    }
    own
}

fn ladder(c: &Value, no_color: bool) -> String {
    let entries = c["ladder"].as_array().cloned().unwrap_or_default();
    entries
        .iter()
        .map(|e| {
            let status = e["status"].as_str().unwrap_or("not_reached");
            if no_color {
                match status {
                    "pass" => "P",
                    "lite" => "L",
                    "fail" => "F",
                    "fault" => "X",
                    "running" => "R",
                    _ => "·",
                }
                .to_string()
            } else {
                match status {
                    "pass" => "■",
                    "lite" => "▣",
                    "fail" => "□",
                    "fault" => "▨",
                    "running" => "▶",
                    _ => "·",
                }
                .to_string()
            }
        })
        .collect()
}

fn running_cells(app: &App) -> usize {
    app.campaigns
        .iter()
        .filter(|c| c["state"].as_str() == Some("running"))
        .count()
}

/// R42-8: grid cells are CLI campaigns, not engine jobs; the field counts
/// the Board's running cells. The engine's own state shows only when it is
/// unreachable.
fn status_field(app: &App) -> String {
    if let Some(e) = &app.engine_error {
        return format!("engine ● unreachable ({e})");
    }
    let running = running_cells(app);
    if running > 0 {
        format!("cells ● {running} running")
    } else {
        "cells ● idle".to_string()
    }
}

/// R42-6: at a narrow width the middle fields drop from the right (image,
/// then rules); the `?help q quit` hint is never dropped.
fn header_line(app: &App, width: u16) -> String {
    let status = status_field(app);
    let rules = app.status["rules_version"].as_str().unwrap_or("—");
    let image = app.status["sandbox_image"].as_str().unwrap_or("—");
    let image = image.rsplit(':').next().unwrap_or(image);
    let paused = app.status["queue_paused"].as_bool().unwrap_or(false);
    let q = app.status["queue"]["pending"].as_u64().unwrap_or(0);
    let tail = format!(
        "queue {q}{} ?help q quit",
        if paused { " (paused)" } else { "" }
    );
    let base = format!("subdollarbench tui · {status}");
    let mut fields = vec![format!("rules {rules}"), format!("image {image}"), tail];
    let render = |fields: &[String]| format!("{base} · {}", fields.join(" · "));
    let mut line = render(&fields);
    while line.chars().count() > width as usize && fields.len() > 1 {
        // Image is first to go, then rules; the queue/hint tail stays.
        let drop = if fields.len() > 2 { 1 } else { 0 };
        fields.remove(drop);
        line = render(&fields);
    }
    line
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(Paragraph::new(header_line(app, area.width)), area);
}

fn render_cells(frame: &mut Frame, app: &App, area: Rect) {
    let narrow = area.width < 100;
    let widths = if narrow {
        vec![
            Constraint::Length(3),
            Constraint::Min(12),
            Constraint::Length(18),
            Constraint::Length(9),
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Length(18),
        ]
    } else {
        vec![
            Constraint::Length(3),
            Constraint::Min(16),
            Constraint::Length(25),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(7),
            Constraint::Length(20),
        ]
    };
    let header = Row::new(vec!["#", "model", "ladder", "pts", "$", "pts/$", "state"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows = app
        .campaigns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let p = if is_career(c) {
                career_points(c, &app.campaigns)
            } else {
                points(c)
            };
            let cost = cost(c);
            let pd = if cost > 0.0 {
                format!("{:.1}", p / cost)
            } else {
                "—".into()
            };
            let strip = ladder(c, std::env::var_os("NO_COLOR").is_some());
            let strip = if narrow && strip.chars().count() > 18 {
                strip.chars().take(18).collect::<String>()
            } else {
                strip
            };
            Row::new(vec![
                Cell::from(format!("{}", i + 1)),
                Cell::from(short_model(c)),
                Cell::from(strip),
                Cell::from(format!("{p:.1}/{:.0}", max_points(c))),
                Cell::from(format!("${cost:.4}")),
                Cell::from(pd),
                Cell::from(state_text(c)),
            ])
        })
        .collect::<Vec<_>>();
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" cells "));
    frame.render_stateful_widget(
        table,
        area,
        &mut ratatui::widgets::TableState::default().with_selected(Some(app.selected)),
    );
}

/// R42-7: minutes since the cell's recorded start, from the API's
/// `started_at` (top level or rollup fallback).
fn elapsed_minutes(c: &Value) -> i64 {
    let started = c["started_at"]
        .as_str()
        .or_else(|| c["rollup"]["started_at"].as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
    started
        .map(|t| {
            (chrono::Utc::now() - t.with_timezone(&chrono::Utc))
                .num_minutes()
                .max(0)
        })
        .unwrap_or(0)
}

fn render_live(frame: &mut Frame, app: &App, area: Rect) {
    let Some(c) = app.selected_cell() else {
        frame.render_widget(Paragraph::new("no cell"), area);
        return;
    };
    let live = c.get("live").unwrap_or(&Value::Null);
    // R55 F2: campaign_spent counts finished tickets; add the live ticket
    // so campaign $ is never below ticket $.
    let mut campaign = live["campaign_spent"].as_f64().unwrap_or_else(|| cost(c));
    if matches!(c["state"].as_str(), Some("running") | Some("stalled")) {
        campaign += live["ticket_spent"].as_f64().unwrap_or(0.0);
    }
    let mut lines = vec![
        Line::from(format!(
            "ticket {} {} · round {} · turn {}/{} · {}",
            live["ticket"].as_u64().unwrap_or(0),
            live["title"].as_str().unwrap_or(""),
            live["round"].as_u64().unwrap_or(0),
            live["turn"].as_u64().unwrap_or(0),
            live["max_turns"].as_u64().unwrap_or(0),
            state_text(c)
        )),
        Line::from(format!(
            "ticket $ {:.4}   campaign $ {:.4} / {:.2}   wall {}/{} m",
            live["ticket_spent"].as_f64().unwrap_or(0.0),
            campaign,
            c["rollup"]["budget_usd"].as_f64().unwrap_or(0.99),
            // R42-7: /api/campaigns serves `started_at` and `wall_cap_min`;
            // the old live.wall_elapsed_min/wall_min fields do not exist.
            elapsed_minutes(c),
            c["wall_cap_min"]
                .as_u64()
                .or_else(|| c["rollup"]["wall_min"].as_u64())
                .unwrap_or(0)
        )),
    ];
    lines.extend(app.filtered_log());
    let p = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(format!(
        " live: {} · {} ",
        short_model(c),
        if app.follow { "following" } else { "paused" }
    )));
    frame.render_widget(p, area);
}

fn render_tickets(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app
        .tickets
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let id = t["id"].as_u64().unwrap_or(i as u64);
            let title = t["title"].as_str().unwrap_or("");
            let status = t["status"].as_str().unwrap_or("");
            let cause = t["cause"].as_str().unwrap_or("");
            Row::new(vec![
                Cell::from(format!("{id} {title}")),
                Cell::from(status.to_string()),
                Cell::from(cause.to_string()),
            ])
        })
        .collect::<Vec<_>>();
    let table = Table::new(
        rows,
        [
            Constraint::Min(18),
            Constraint::Length(12),
            Constraint::Min(12),
        ],
    )
    .header(Row::new(vec!["ticket", "status", "cause"]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" tickets · Esc back "),
    );
    frame.render_stateful_widget(
        table,
        area,
        &mut ratatui::widgets::TableState::default().with_selected(Some(app.selected_ticket)),
    );
}

fn render_help(frame: &mut Frame, area: Rect) {
    let text = [
        "↑/↓ select   Enter tickets   Esc back",
        "Tab focus   v verdict   a agent   t tools   s setup",
        "End follow   ? help   q quit",
    ]
    .join("\n");
    let popup = centered(area, 60, 7);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" help ")),
        popup,
    );
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w.min(area.width),
        height: h.min(area.height),
    }
}

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Percentage(45),
            Constraint::Min(8),
        ])
        .split(area);
    render_header(frame, app, chunks[0]);
    if app.view == View::Tickets {
        render_tickets(frame, app, chunks[2]);
    } else {
        render_cells(frame, app, chunks[1]);
        render_live(frame, app, chunks[2]);
    }
    if app.help {
        render_help(frame, area);
    }
}

pub(crate) fn restore_terminal() {
    RESTORE_CALLS.fetch_add(1, Ordering::SeqCst);
    let _ = disable_raw_mode();
    let mut out = stdout();
    let _ = execute!(out, LeaveAlternateScreen);
    let _ = out.flush();
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

/// Poll loop. The terminal guard is deliberately outside every possible
/// early return.
pub async fn run(server_arg: Option<String>, interval: u64) -> Result<()> {
    let interval = interval.max(1);
    install_panic_hook();
    enable_raw_mode().context("enable raw mode")?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen).context("enter alternate screen")?;
    let _guard = RestoreGuard;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend).context("terminal")?;
    let server = crate::remote::resolve_server(server_arg.as_deref());
    let mut app = App::new();
    let mut last = Instant::now() - Duration::from_secs(interval);
    loop {
        if last.elapsed() >= Duration::from_secs(interval) {
            match crate::remote::get_json(&server, "/api/status").await {
                Ok(v) => app.set_status(v),
                Err(e) => app.unreachable(e.to_string()),
            }
            if let Ok(v) = crate::remote::get_json(&server, "/api/campaigns").await {
                app.set_campaigns(v.as_array().cloned().unwrap_or_default());
            }
            if let Some(id) = app.selected_id().map(str::to_string) {
                let after = app.log_offset();
                let path = app.log_request_path(after).unwrap();
                if let Ok(v) = crate::remote::get_json(&server, &path).await {
                    let off = v["offset"].as_u64().unwrap_or(after);
                    let lines = v["lines"].as_array().cloned().unwrap_or_default();
                    app.apply_log_page(off, lines);
                }
                if app.view == View::Tickets && app.tickets.is_empty() {
                    if let Ok(v) =
                        crate::remote::get_json(&server, &format!("/api/campaigns/{id}")).await
                    {
                        app.set_tickets(v["ladder"].as_array().cloned().unwrap_or_default());
                    }
                }
            }
            last = Instant::now();
        }
        terminal.draw(|f| render(f, &app))?;
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press && app.handle_key(k.code, k.modifiers) {
                    break;
                }
            }
        }
    }
    Ok(())
}

struct RestoreGuard;
impl Drop for RestoreGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

#[cfg(test)]
mod tests;

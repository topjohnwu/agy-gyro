// SPDX-License-Identifier: MIT

use crate::config::MonitorArgs;
use crate::usage::{LogEntry, ModelUsageStats, UsageSnapshot};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
};
use std::io;
use std::time::{Duration, Instant};

/// Normalizes a gateway base URL or endpoint URL into a full `.../usage` endpoint URL.
pub fn resolve_usage_endpoint_url(raw_url: &str) -> String {
    let trimmed = raw_url.trim().trim_end_matches('/');
    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{}", trimmed)
    };

    if let Some(base) = with_scheme.strip_suffix("/logs") {
        format!("{}/usage", base)
    } else if with_scheme.ends_with("/usage") {
        with_scheme
    } else {
        format!("{}/usage", with_scheme)
    }
}

/// Normalizes a gateway base URL or endpoint URL into a full `.../logs` endpoint URL.
pub fn resolve_logs_endpoint_url(raw_url: &str) -> String {
    let trimmed = raw_url.trim().trim_end_matches('/');
    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{}", trimmed)
    };

    if let Some(base) = with_scheme.strip_suffix("/usage") {
        format!("{}/logs", base)
    } else if with_scheme.ends_with("/logs") {
        with_scheme
    } else {
        format!("{}/logs", with_scheme)
    }
}

/// Drains complete `\n`-terminated SSE `data:` JSON payloads from a raw byte buffer,
/// preserving any incomplete trailing bytes (including split multi-byte UTF-8 sequences).
fn drain_sse_data_line(buf: &mut Vec<u8>) -> Option<String> {
    while let Some(newline_idx) = buf.iter().position(|&b| b == b'\n') {
        let line = String::from_utf8_lossy(&buf[..newline_idx])
            .trim()
            .to_string();
        buf.drain(..=newline_idx);
        if let Some(data) = line.strip_prefix("data:") {
            let json_str = data.trim();
            if !json_str.is_empty() {
                return Some(json_str.to_string());
            }
        }
    }
    None
}

/// Connects to the SSE `GET /usage` stream and reads the initial [`UsageSnapshot`] event.
pub async fn fetch_usage_snapshot(
    client: &reqwest::Client,
    endpoint_url: &str,
) -> Result<UsageSnapshot, String> {
    let res = client
        .get(endpoint_url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .map_err(|e| format!("Connection error: {}", e))?;

    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status()));
    }

    let mut stream = res.bytes_stream();
    let mut buf = Vec::new();

    while let Some(chunk_res) = stream.next().await {
        let chunk = chunk_res.map_err(|e| format!("SSE read error: {}", e))?;
        buf.extend_from_slice(&chunk);

        if let Some(json_str) = drain_sse_data_line(&mut buf) {
            return serde_json::from_str::<UsageSnapshot>(&json_str)
                .map_err(|e| format!("Invalid SSE JSON payload: {}", e));
        }
    }

    Err("SSE stream closed before emitting a snapshot".to_string())
}

/// Connects to the SSE `GET /logs` stream and reads the initial `Vec<LogEntry>` event.
pub async fn fetch_logs_snapshot(
    client: &reqwest::Client,
    logs_url: &str,
) -> Result<Vec<LogEntry>, String> {
    let res = client
        .get(logs_url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .map_err(|e| format!("Connection error: {}", e))?;

    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status()));
    }

    let mut stream = res.bytes_stream();
    let mut buf = Vec::new();

    while let Some(chunk_res) = stream.next().await {
        let chunk = chunk_res.map_err(|e| format!("SSE read error: {}", e))?;
        buf.extend_from_slice(&chunk);

        if let Some(json_str) = drain_sse_data_line(&mut buf) {
            return serde_json::from_str::<Vec<LogEntry>>(&json_str)
                .map_err(|e| format!("Invalid SSE JSON payload: {}", e));
        }
    }

    Err("SSE stream closed before emitting logs".to_string())
}

/// Resets usage statistics on the target `agy-gyro` instance via `DELETE /usage`.
pub async fn reset_remote_usage(
    client: &reqwest::Client,
    endpoint_url: &str,
) -> Result<UsageSnapshot, String> {
    let res = client
        .delete(endpoint_url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .map_err(|e| format!("Reset failed: {}", e))?;

    if !res.status().is_success() {
        return Err(format!("Reset returned HTTP {}", res.status()));
    }

    res.json::<UsageSnapshot>()
        .await
        .map_err(|e| format!("Invalid JSON response: {}", e))
}

/// State held by the TUI monitor dashboard.
pub struct DashboardState {
    pub endpoint_url: String,
    pub snapshot: Option<UsageSnapshot>,
    pub logs: Vec<LogEntry>,
    pub connected: bool,
    pub last_error: Option<String>,
    pub status_message: Option<(String, Instant)>,
    pub log_scroll_from_bottom: usize,
    pub log_viewport_height: usize,
    pub tick_count: u64,
}

impl DashboardState {
    pub fn new(args: &MonitorArgs) -> Self {
        let endpoint_url = args.resolve_endpoint_url();
        Self {
            endpoint_url,
            snapshot: None,
            logs: Vec::new(),
            connected: false,
            last_error: None,
            status_message: None,
            log_scroll_from_bottom: 0,
            log_viewport_height: 10,
            tick_count: 0,
        }
    }

    pub fn sorted_models(&self) -> Vec<&ModelUsageStats> {
        let Some(ref snap) = self.snapshot else {
            return Vec::new();
        };
        let mut list: Vec<&ModelUsageStats> = snap.models.values().collect();
        list.sort_by(|a, b| {
            b.totals
                .total_cost_usd
                .partial_cmp(&a.totals.total_cost_usd)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.totals.total_tokens.cmp(&a.totals.total_tokens))
                .then_with(|| a.model.cmp(&b.model))
        });
        list
    }

    /// Maximum number of lines the log panel can scroll up from the bottom
    /// while keeping the viewport full (`logs[0]` at the top row).
    pub fn max_log_scroll(&self) -> usize {
        self.logs
            .len()
            .saturating_sub(self.log_viewport_height.max(1))
    }

    pub fn update_viewport_size(&mut self, area: Rect) {
        let chunks = dashboard_chunks(area);
        self.log_viewport_height = chunks[3].height.saturating_sub(2) as usize;
        let max_scroll = self.max_log_scroll();
        if self.log_scroll_from_bottom > max_scroll {
            self.log_scroll_from_bottom = max_scroll;
        }
    }

    pub fn scroll_logs_up(&mut self, lines: usize) {
        let max_scroll = self.max_log_scroll();
        self.log_scroll_from_bottom = (self.log_scroll_from_bottom + lines).min(max_scroll);
    }

    pub fn scroll_logs_down(&mut self, lines: usize) {
        let max_scroll = self.max_log_scroll();
        self.log_scroll_from_bottom = self
            .log_scroll_from_bottom
            .min(max_scroll)
            .saturating_sub(lines);
    }

    /// Updates the log buffer while keeping the viewport anchored on the same log lines when paused.
    pub fn set_logs(&mut self, new_logs: Vec<LogEntry>) {
        if self.log_scroll_from_bottom > 0 && !self.logs.is_empty() {
            let max_prev_scroll = self.max_log_scroll();
            let effective_prev_scroll = self.log_scroll_from_bottom.min(max_prev_scroll);
            let prev_end_idx = self.logs.len().saturating_sub(effective_prev_scroll);
            if prev_end_idx > 0
                && let Some(anchor_id) = self.logs.get(prev_end_idx - 1).map(|l| l.id)
            {
                self.logs = new_logs;
                if let Some(pos) = self.logs.iter().position(|l| l.id == anchor_id) {
                    let new_scroll = self.logs.len().saturating_sub(pos + 1);
                    self.log_scroll_from_bottom = new_scroll.min(self.max_log_scroll());
                } else {
                    self.log_scroll_from_bottom = self.max_log_scroll();
                }
                return;
            }
        }
        self.logs = new_logs;
        let max_scroll = self.max_log_scroll();
        if self.log_scroll_from_bottom > max_scroll {
            self.log_scroll_from_bottom = max_scroll;
        }
    }
}

/// Formats an integer with thousands separators (e.g. `1234567` -> `"1,234,567"`).
pub fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (idx, ch) in s.chars().rev().enumerate() {
        if idx > 0 && idx % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

/// Formats a USD price value with appropriate precision.
pub fn format_usd(amount: f64) -> String {
    if amount == 0.0 {
        "$0.0000".to_string()
    } else if amount < 0.01 {
        format!("${:.6}", amount)
    } else if amount < 10.0 {
        format!("${:.4}", amount)
    } else {
        format!("${:.2}", amount)
    }
}

/// Formats seconds into `HH:MM:SS`.
pub fn format_duration_secs(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

fn dashboard_chunks(area: Rect) -> std::rc::Rc<[Rect]> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // Header bar
            Constraint::Length(6),  // 4 Summary cards
            Constraint::Length(10), // Per-model breakdown table
            Constraint::Min(8),     // Real-time gateway logs
            Constraint::Length(1),  // Footer keybindings bar
        ])
        .split(area)
}

/// Renders the complete TUI dashboard frame from [`DashboardState`].
pub fn render_dashboard(frame: &mut Frame, state: &DashboardState) {
    let chunks = dashboard_chunks(frame.area());

    render_header(frame, chunks[0], state);
    render_summary_cards(frame, chunks[1], state);
    render_models_table(frame, chunks[2], state);
    render_logs_panel(frame, chunks[3], state);
    render_footer(frame, chunks[4], state);
}

fn render_header(frame: &mut Frame, area: Rect, state: &DashboardState) {
    let spinner_frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let spinner = spinner_frames[(state.tick_count as usize) % spinner_frames.len()];

    let (status_span, uptime_str, pricing_src, active_span) =
        match (&state.snapshot, state.connected) {
            (Some(snap), true) => {
                let active = if snap.active_requests > 0 {
                    Span::styled(
                        format!(" {} {} in-flight ", spinner, snap.active_requests),
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::styled(" idle ", Style::default().fg(Color::DarkGray))
                };
                (
                    Span::styled(
                        " ● CONNECTED ",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    format_duration_secs(snap.uptime_secs),
                    snap.pricing_source.as_str(),
                    active,
                )
            }
            _ => (
                Span::styled(
                    " ○ DISCONNECTED ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                "--:--:--".to_string(),
                "n/a",
                Span::styled(" waiting ", Style::default().fg(Color::DarkGray)),
            ),
        };

    let header_line = Line::from(vec![
        Span::styled(
            " agy-gyro ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        status_span,
        Span::styled(" │ Endpoint: ", Style::default().fg(Color::DarkGray)),
        Span::styled(&state.endpoint_url, Style::default().fg(Color::White)),
        Span::styled(" │ Pricing: ", Style::default().fg(Color::DarkGray)),
        Span::styled(pricing_src, Style::default().fg(Color::Magenta)),
        Span::styled(" │ Uptime: ", Style::default().fg(Color::DarkGray)),
        Span::styled(uptime_str, Style::default().fg(Color::White)),
        Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
        active_span,
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Antigravity Enterprise Gateway — Live Token & Price Monitor ");

    let paragraph = Paragraph::new(header_line).block(block);
    frame.render_widget(paragraph, area);
}

fn render_summary_cards(frame: &mut Frame, area: Rect, state: &DashboardState) {
    let cards = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(area);

    let totals = state
        .snapshot
        .as_ref()
        .map(|s| s.totals.clone())
        .unwrap_or_default();

    let cache_hit_pct = if totals.input_tokens > 0 {
        (totals.cached_input_tokens as f64 / totals.input_tokens as f64) * 100.0
    } else {
        0.0
    };

    let think_pct = if totals.output_tokens > 0 {
        (totals.thoughts_tokens as f64 / totals.output_tokens as f64) * 100.0
    } else {
        0.0
    };

    let (ok_reqs, failed_reqs, total_retries) = state
        .snapshot
        .as_ref()
        .map(|s| (s.successful_requests, s.failed_requests, s.total_retries))
        .unwrap_or((0, 0, 0));

    // Card 1: Input Tokens
    let input_card = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Total Input: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format_number(totals.input_tokens),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Uncached:    ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format_number(totals.uncached_input_tokens),
                Style::default().fg(Color::Cyan),
            ),
        ]),
        Line::from(vec![
            Span::styled("Uncached $:  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format_usd(totals.uncached_input_cost_usd),
                Style::default().fg(Color::Yellow),
            ),
        ]),
        Line::from(vec![
            Span::styled("Total In $:  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format_usd(totals.input_cost_usd),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Blue))
            .title(" Input Tokens "),
    );
    frame.render_widget(input_card, cards[0]);

    // Card 2: Cached Input Tokens
    let cached_card = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Cached Tok:  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format_number(totals.cached_input_tokens),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Cache Hit:   ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:.1}%", cache_hit_pct),
                Style::default().fg(Color::Cyan),
            ),
        ]),
        Line::from(vec![
            Span::styled("Cached $:    ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format_usd(totals.cached_input_cost_usd),
                Style::default().fg(Color::Yellow),
            ),
        ]),
        Line::from(vec![
            Span::styled("Cache Saved: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format_usd(totals.cache_savings_usd),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Magenta))
            .title(" Cached Input Tokens "),
    );
    frame.render_widget(cached_card, cards[1]);

    // Card 3: Output Tokens
    let output_card = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Output Tok:  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format_number(totals.output_tokens),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Candidates:  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format_number(totals.candidates_tokens),
                Style::default().fg(Color::Cyan),
            ),
        ]),
        Line::from(vec![
            Span::styled("Thinking:    ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!(
                    "{} ({:.0}%)",
                    format_number(totals.thoughts_tokens),
                    think_pct
                ),
                Style::default().fg(Color::LightYellow),
            ),
        ]),
        Line::from(vec![
            Span::styled("Output $:    ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format_usd(totals.output_cost_usd),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(" Output & Thinking Tokens "),
    );
    frame.render_widget(output_card, cards[2]);

    // Card 4: Total Cost & Requests
    let total_card = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Total Price: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format_usd(totals.total_cost_usd),
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("All Tokens:  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format_number(totals.total_tokens),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("Requests:    ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} ok / {} err", ok_reqs, failed_reqs),
                Style::default().fg(if failed_reqs > 0 {
                    Color::LightRed
                } else {
                    Color::Cyan
                }),
            ),
        ]),
        Line::from(vec![
            Span::styled("Retries:     ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format_number(total_retries),
                Style::default().fg(if total_retries > 0 {
                    Color::Yellow
                } else {
                    Color::DarkGray
                }),
            ),
        ]),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Green))
            .title(" Total Price & Reliability "),
    );
    frame.render_widget(total_card, cards[3]);
}

fn render_models_table(frame: &mut Frame, area: Rect, state: &DashboardState) {
    let header_cells = [
        "Model",
        "Reqs",
        "Input Tok",
        "Uncached In",
        "Cached In (Hit%)",
        "Output (Think)",
        "Input $",
        "Cached $",
        "Output $",
        "Total $",
        "Rates ($/1M In/Cache/Out)",
    ]
    .into_iter()
    .map(|h| {
        Cell::from(h).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    });

    let header = Row::new(header_cells).height(1).bottom_margin(1);
    let models = state.sorted_models();

    let rows: Vec<Row> = if models.is_empty() {
        vec![Row::new(vec![
            Cell::from("No model requests recorded yet"),
            Cell::from("-"),
            Cell::from("0"),
            Cell::from("0"),
            Cell::from("0 (0.0%)"),
            Cell::from("0 (0)"),
            Cell::from("$0.0000"),
            Cell::from("$0.0000"),
            Cell::from("$0.0000"),
            Cell::from("$0.0000"),
            Cell::from("-"),
        ])]
    } else {
        models
            .iter()
            .map(|m| {
                let t = &m.totals;
                let hit_pct = if t.input_tokens > 0 {
                    (t.cached_input_tokens as f64 / t.input_tokens as f64) * 100.0
                } else {
                    0.0
                };
                let req_str = if t.retries > 0 {
                    format!("{} ({}↻)", t.requests, t.retries)
                } else {
                    t.requests.to_string()
                };
                let rates_str = match &m.pricing {
                    Some(p) => format!(
                        "${:.2} / ${:.3} / ${:.2}",
                        p.input_per_1m, p.cached_per_1m, p.output_per_1m
                    ),
                    None => "n/a".to_string(),
                };

                Row::new(vec![
                    Cell::from(m.model.clone()).style(
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Cell::from(req_str),
                    Cell::from(format_number(t.input_tokens)),
                    Cell::from(format_number(t.uncached_input_tokens)),
                    Cell::from(format!(
                        "{} ({:.0}%)",
                        format_number(t.cached_input_tokens),
                        hit_pct
                    ))
                    .style(Style::default().fg(Color::Magenta)),
                    Cell::from(format!(
                        "{} ({})",
                        format_number(t.output_tokens),
                        format_number(t.thoughts_tokens)
                    )),
                    Cell::from(format_usd(t.uncached_input_cost_usd)),
                    Cell::from(format_usd(t.cached_input_cost_usd)),
                    Cell::from(format_usd(t.output_cost_usd)),
                    Cell::from(format_usd(t.total_cost_usd)).style(
                        Style::default()
                            .fg(Color::LightGreen)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Cell::from(rates_str).style(Style::default().fg(Color::DarkGray)),
                ])
            })
            .collect()
    };

    let widths = [
        Constraint::Min(20),
        Constraint::Length(8),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(17),
        Constraint::Length(16),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(25),
    ];

    let table = Table::new(rows, widths).header(header).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::White))
            .title(" Per-Model Token Usage & Price Breakdown "),
    );

    frame.render_widget(table, area);
}

fn render_logs_panel(frame: &mut Frame, area: Rect, state: &DashboardState) {
    let logs = state.logs.as_slice();

    let visible_rows = area.height.saturating_sub(2) as usize;
    let total = logs.len();
    let max_scroll = total.saturating_sub(visible_rows);
    let effective_scroll = state.log_scroll_from_bottom.min(max_scroll);
    let end_idx = total.saturating_sub(effective_scroll);
    let start_idx = end_idx.saturating_sub(visible_rows);

    let lines: Vec<Line> = if logs.is_empty() {
        vec![Line::from(Span::styled(
            "Waiting for gateway logs...",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        logs[start_idx..end_idx]
            .iter()
            .map(|entry| {
                let (level_str, level_style) = match entry.level.as_str() {
                    "ERROR" => (
                        "ERROR",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ),
                    "WARN" => (
                        "WARN ",
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    "INFO" => ("INFO ", Style::default().fg(Color::Green)),
                    "DEBUG" => ("DEBUG", Style::default().fg(Color::Cyan)),
                    other => (other, Style::default().fg(Color::DarkGray)),
                };

                Line::from(vec![
                    Span::styled(
                        format!("[{}] ", format_duration_secs(entry.timestamp_secs)),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(format!("{} ", level_str), level_style),
                    Span::styled(entry.message.clone(), Style::default().fg(Color::White)),
                ])
            })
            .collect()
    };

    let title = if effective_scroll > 0 {
        format!(
            " Live Gateway Logs (paused -{} lines, ↑/↓/PgUp/PgDn to scroll) ",
            effective_scroll
        )
    } else {
        format!(
            " Live Gateway Logs ({} entries, ↑/↓/PgUp/PgDn to scroll) ",
            total
        )
    };

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Blue))
            .title(title),
    );

    frame.render_widget(paragraph, area);
}

fn render_footer(frame: &mut Frame, area: Rect, state: &DashboardState) {
    let mut spans = vec![
        Span::styled(
            " q / Esc ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" Quit  "),
        Span::styled(
            " r ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" Reset Stats  "),
        Span::styled(
            " ↑/↓ / PgUp/PgDn ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" Scroll Logs  "),
        Span::styled("│ Stream: SSE ", Style::default().fg(Color::DarkGray)),
    ];

    if let Some((ref msg, ts)) = state.status_message
        && ts.elapsed() < Duration::from_secs(4)
    {
        spans.push(Span::styled(
            format!("│ {} ", msg),
            Style::default()
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
        ));
    } else if let Some(ref err) = state.last_error {
        spans.push(Span::styled(
            format!("│ {} ", err),
            Style::default().fg(Color::Yellow),
        ));
    }

    let footer = Paragraph::new(Line::from(spans)).alignment(Alignment::Left);
    frame.render_widget(footer, area);
}

enum SseUpdate {
    Snapshot(Box<UsageSnapshot>),
    Logs(Vec<LogEntry>),
    Disconnected(String),
}

fn spawn_usage_sse_listener(
    client: reqwest::Client,
    usage_url: String,
    tx: tokio::sync::mpsc::UnboundedSender<SseUpdate>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match client.get(&usage_url).send().await {
                Ok(res) if res.status().is_success() => {
                    let mut stream = res.bytes_stream();
                    let mut buf = Vec::new();
                    let mut stream_err = "SSE stream closed".to_string();

                    loop {
                        match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
                            Ok(Some(Ok(chunk))) => {
                                buf.extend_from_slice(&chunk);
                                while let Some(json_str) = drain_sse_data_line(&mut buf) {
                                    if let Ok(snapshot) =
                                        serde_json::from_str::<UsageSnapshot>(&json_str)
                                        && tx.send(SseUpdate::Snapshot(Box::new(snapshot))).is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                            Ok(Some(Err(e))) => {
                                stream_err = format!("SSE stream error: {}", e);
                                break;
                            }
                            Ok(None) => break,
                            Err(_) => {
                                stream_err = "SSE stream timed out".to_string();
                                break;
                            }
                        }
                    }

                    if tx.send(SseUpdate::Disconnected(stream_err)).is_err() {
                        return;
                    }
                }
                Ok(res) => {
                    if tx
                        .send(SseUpdate::Disconnected(format!("HTTP {}", res.status())))
                        .is_err()
                    {
                        return;
                    }
                }
                Err(e) => {
                    if tx
                        .send(SseUpdate::Disconnected(format!("Connection error: {}", e)))
                        .is_err()
                    {
                        return;
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
}

fn spawn_logs_sse_listener(
    client: reqwest::Client,
    logs_url: String,
    tx: tokio::sync::mpsc::UnboundedSender<SseUpdate>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Ok(res) = client.get(&logs_url).send().await
                && res.status().is_success()
            {
                let mut stream = res.bytes_stream();
                let mut buf = Vec::new();

                while let Ok(Some(Ok(chunk))) =
                    tokio::time::timeout(Duration::from_secs(30), stream.next()).await
                {
                    buf.extend_from_slice(&chunk);
                    while let Some(json_str) = drain_sse_data_line(&mut buf) {
                        if let Ok(logs) = serde_json::from_str::<Vec<LogEntry>>(&json_str)
                            && tx.send(SseUpdate::Logs(logs)).is_err()
                        {
                            return;
                        }
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
}

struct TerminalRestoreGuard;

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
    }
}

/// Runs the interactive live TUI usage and cost monitor dashboard over SSE.
pub async fn run_monitor(args: MonitorArgs) -> Result<(), Box<dyn std::error::Error>> {
    let sse_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .tcp_keepalive(Duration::from_secs(15))
        .build()?;
    let cmd_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;

    let mut state = DashboardState::new(&args);
    let logs_url = resolve_logs_endpoint_url(&state.endpoint_url);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let usage_sse_handle =
        spawn_usage_sse_listener(sse_client.clone(), state.endpoint_url.clone(), tx.clone());
    let logs_sse_handle = spawn_logs_sse_listener(sse_client, logs_url, tx);

    enable_raw_mode()?;
    let _restore_guard = TerminalRestoreGuard;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_monitor_loop(&mut terminal, &cmd_client, &mut state, rx).await;

    usage_sse_handle.abort();
    logs_sse_handle.abort();

    res
}

async fn run_monitor_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    cmd_client: &reqwest::Client,
    state: &mut DashboardState,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<SseUpdate>,
) -> Result<(), Box<dyn std::error::Error>> {
    let tick_rate = Duration::from_millis(100);

    loop {
        while let Ok(update) = rx.try_recv() {
            match update {
                SseUpdate::Snapshot(snapshot) => {
                    state.snapshot = Some(*snapshot);
                    state.connected = true;
                    state.last_error = None;
                }
                SseUpdate::Logs(logs) => {
                    state.set_logs(logs);
                }
                SseUpdate::Disconnected(error) => {
                    state.connected = false;
                    state.last_error = Some(error);
                }
            }
        }

        terminal.draw(|frame| {
            state.update_viewport_size(frame.area());
            render_dashboard(frame, state);
        })?;

        if event::poll(tick_rate)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(());
                }
                KeyCode::Up | KeyCode::Char('k') => state.scroll_logs_up(1),
                KeyCode::Down | KeyCode::Char('j') => state.scroll_logs_down(1),
                KeyCode::PageUp | KeyCode::Char('[') => state.scroll_logs_up(5),
                KeyCode::PageDown | KeyCode::Char(']') => state.scroll_logs_down(5),
                KeyCode::Home | KeyCode::Char('g') => {
                    state.log_scroll_from_bottom = state.max_log_scroll();
                }
                KeyCode::End | KeyCode::Char('G') => {
                    state.log_scroll_from_bottom = 0;
                }
                KeyCode::Char('r') => {
                    match reset_remote_usage(cmd_client, &state.endpoint_url).await {
                        Ok(snap) => {
                            state.snapshot = Some(snap);
                            state.logs.clear();
                            state.connected = true;
                            state.last_error = None;
                            state.log_scroll_from_bottom = 0;
                            state.status_message =
                                Some(("Usage statistics reset.".to_string(), Instant::now()));
                        }
                        Err(err) => {
                            state.last_error = Some(err);
                        }
                    }
                }
                _ => {}
            }
        }

        state.tick_count = state.tick_count.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{LogEntry, ModelPricing, TokenUsageTotals};
    use ratatui::backend::TestBackend;
    use std::collections::BTreeMap;

    #[test]
    fn test_resolve_endpoint_urls() {
        assert_eq!(
            resolve_usage_endpoint_url("127.0.0.1:6470"),
            "http://127.0.0.1:6470/usage"
        );
        assert_eq!(
            resolve_usage_endpoint_url("http://127.0.0.1:9000/"),
            "http://127.0.0.1:9000/usage"
        );
        assert_eq!(
            resolve_usage_endpoint_url("http://127.0.0.1:9000/usage"),
            "http://127.0.0.1:9000/usage"
        );
        assert_eq!(
            resolve_logs_endpoint_url("127.0.0.1:6470"),
            "http://127.0.0.1:6470/logs"
        );
        assert_eq!(
            resolve_logs_endpoint_url("http://127.0.0.1:9000/usage"),
            "http://127.0.0.1:9000/logs"
        );
    }

    #[test]
    fn test_format_helpers() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1_234_567), "1,234,567");
        assert_eq!(format_usd(0.0), "$0.0000");
        assert_eq!(format_usd(0.1234), "$0.1234");
        assert_eq!(format_duration_secs(3661), "01:01:01");
    }

    #[test]
    fn test_render_dashboard_with_test_backend() {
        let args = MonitorArgs {
            url: Some("http://127.0.0.1:6470".to_string()),
            host: "127.0.0.1".to_string(),
            port: 6470,
        };
        let mut state = DashboardState::new(&args);
        state.connected = true;

        let mut models = BTreeMap::new();
        models.insert(
            "gemini-4-argon".to_string(),
            ModelUsageStats {
                model: "gemini-4-argon".to_string(),
                totals: TokenUsageTotals {
                    requests: 5,
                    retries: 1,
                    input_tokens: 150_000,
                    uncached_input_tokens: 30_000,
                    cached_input_tokens: 120_000,
                    output_tokens: 10_000,
                    candidates_tokens: 4_000,
                    thoughts_tokens: 6_000,
                    total_tokens: 160_000,
                    uncached_input_cost_usd: 0.06,
                    cached_input_cost_usd: 0.012,
                    input_cost_usd: 0.072,
                    output_cost_usd: 0.10,
                    total_cost_usd: 0.172,
                    cache_savings_usd: 0.228,
                },
                pricing: Some(ModelPricing::flat(2.0, 0.1, 10.0)),
            },
        );

        state.snapshot = Some(UsageSnapshot {
            uptime_secs: 95,
            active_requests: 1,
            total_requests: 5,
            successful_requests: 5,
            failed_requests: 0,
            total_retries: 1,
            pricing_source: "models.dev (live)".to_string(),
            totals: models["gemini-4-argon"].totals.clone(),
            models,
        });
        state.logs = vec![LogEntry {
            id: 1,
            timestamp_secs: 90,
            level: "INFO".to_string(),
            target: "agy_gyro::proxy".to_string(),
            message: "Request #5 [gemini-4-argon] status=200".to_string(),
        }];

        let backend = TestBackend::new(140, 38);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_dashboard(frame, &state))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let mut rendered = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                rendered.push_str(buffer[(x, y)].symbol());
            }
            rendered.push('\n');
        }

        assert!(rendered.contains("CONNECTED"));
        assert!(rendered.contains("gemini-4-argon"));
        assert!(rendered.contains("150,000"));
        assert!(rendered.contains("120,000"));
        assert!(rendered.contains("$0.1720"));
        assert!(!rendered.contains("Recent Requests Activity"));
        assert!(rendered.contains("Live Gateway Logs"));
        assert!(rendered.contains("Request #5 [gemini-4-argon] status=200"));
    }

    #[test]
    fn test_log_scrolling_bounds_at_top_and_bottom() {
        let args = MonitorArgs {
            url: Some("http://127.0.0.1:6470".to_string()),
            host: "127.0.0.1".to_string(),
            port: 6470,
        };
        let mut state = DashboardState::new(&args);
        state.log_viewport_height = 5;

        // 1. Fewer logs than viewport height: scrolling up should stay at 0
        state.logs = (1..=3)
            .map(|id| LogEntry {
                id,
                timestamp_secs: id,
                level: "INFO".to_string(),
                target: "agy_gyro".to_string(),
                message: format!("Log {}", id),
            })
            .collect();
        state.scroll_logs_up(1);
        assert_eq!(state.log_scroll_from_bottom, 0);

        // 2. More logs than viewport height (12 logs, viewport 5 -> max_scroll = 7)
        state.logs = (1..=12)
            .map(|id| LogEntry {
                id,
                timestamp_secs: id,
                level: "INFO".to_string(),
                target: "agy_gyro".to_string(),
                message: format!("Log {}", id),
            })
            .collect();
        state.scroll_logs_up(100);
        assert_eq!(state.log_scroll_from_bottom, 7);

        // Scrolling down 1 line from the very top immediately moves to 6
        state.scroll_logs_down(1);
        assert_eq!(state.log_scroll_from_bottom, 6);

        // Paused viewport stays anchored on the same lines when new logs arrive
        let new_logs: Vec<LogEntry> = (1..=15)
            .map(|id| LogEntry {
                id,
                timestamp_secs: id,
                level: "INFO".to_string(),
                target: "agy_gyro".to_string(),
                message: format!("Log {}", id),
            })
            .collect();
        state.set_logs(new_logs);
        assert_eq!(state.log_scroll_from_bottom, 9);
    }
}

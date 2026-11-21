// SPDX-License-Identifier: MIT

use crate::config::Config;
use crate::proxy::SetBufferRequest;
use crate::usage::{
    LogEntry, LogStreamEvent, MAX_LOG_ENTRIES, ModelUsageStats, ScopeUsageStats, SessionUsageStats,
    UsageSnapshot, short_session_id,
};
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
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Drains complete `\n`-terminated SSE `data:` JSON payloads from a raw byte buffer,
/// preserving any incomplete trailing bytes (including split multi-byte UTF-8 sequences).
fn drain_sse_data_line(buf: &mut Vec<u8>) -> Option<String> {
    while let Some(newline_idx) = buf.iter().position(|&b| b == b'\n') {
        let extracted = {
            let line = String::from_utf8_lossy(&buf[..newline_idx]);
            line.trim()
                .strip_prefix("data:")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        buf.drain(..=newline_idx);
        if extracted.is_some() {
            return extracted;
        }
    }
    None
}

async fn fetch_sse_first_event<T: DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> Result<T, String> {
    let res = client
        .get(url)
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
            return serde_json::from_str::<T>(&json_str)
                .map_err(|e| format!("Invalid SSE JSON payload: {}", e));
        }
    }

    Err("SSE stream closed before emitting an event".to_string())
}

/// Connects to the SSE `GET /usage` stream and reads the initial [`UsageSnapshot`] event.
pub async fn fetch_usage_snapshot(
    client: &reqwest::Client,
    endpoint_url: &str,
) -> Result<UsageSnapshot, String> {
    fetch_sse_first_event(client, endpoint_url).await
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WireLogEvent {
    Event(LogStreamEvent),
    LegacySnapshot(Vec<LogEntry>),
}

impl From<WireLogEvent> for LogStreamEvent {
    fn from(wire: WireLogEvent) -> Self {
        match wire {
            WireLogEvent::Event(event) => event,
            WireLogEvent::LegacySnapshot(entries) => LogStreamEvent::Snapshot { entries },
        }
    }
}

/// Connects to the SSE `GET /logs` stream and reads the initial log snapshot event.
pub async fn fetch_logs_snapshot(
    client: &reqwest::Client,
    logs_url: &str,
) -> Result<Vec<LogEntry>, String> {
    let event: WireLogEvent = fetch_sse_first_event(client, logs_url).await?;
    let entries = match LogStreamEvent::from(event) {
        LogStreamEvent::Snapshot { entries } | LogStreamEvent::Append { entries } => entries,
    };
    Ok(entries)
}

/// Resets usage statistics on the target `agy-gyro` instance via `DELETE /usage`.
pub async fn reset_remote_usage(
    client: &reqwest::Client,
    endpoint_url: &str,
) -> Result<UsageSnapshot, String> {
    reset_remote_session_usage(client, endpoint_url, None).await
}

/// Resets usage statistics for all sessions (`None`) or a single session (`Some(session_id)`)
/// via `DELETE /usage` or `DELETE /usage?session=<id>`.
pub async fn reset_remote_session_usage(
    client: &reqwest::Client,
    endpoint_url: &str,
    session_id: Option<&str>,
) -> Result<UsageSnapshot, String> {
    let url = match session_id {
        Some(sid) => format!("{}?session={}", endpoint_url, sid),
        None => endpoint_url.to_string(),
    };
    let res = client
        .delete(&url)
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

/// Toggles (`None`) or sets (`Some(enabled)`) full SSE buffering on the running daemon via `POST /buffer`.
pub async fn toggle_remote_buffer(
    client: &reqwest::Client,
    buffer_url: &str,
    enabled: Option<bool>,
) -> Result<UsageSnapshot, String> {
    let res = client
        .post(buffer_url)
        .json(&SetBufferRequest { enabled })
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .map_err(|e| format!("Buffer toggle failed: {}", e))?;

    if !res.status().is_success() {
        return Err(format!("Buffer toggle returned HTTP {}", res.status()));
    }

    res.json::<UsageSnapshot>()
        .await
        .map_err(|e| format!("Invalid JSON response: {}", e))
}

struct WrappedLogsCache {
    selected_session: Option<String>,
    inner_width: usize,
    log_count: usize,
    first_log_id: Option<u64>,
    last_log_id: Option<u64>,
    visible_entry_count: usize,
    lines: VecDeque<(u64, usize, Line<'static>)>,
}

/// State held by the TUI monitor dashboard.
pub struct DashboardState {
    pub endpoint_url: String,
    pub buffer_url: String,
    pub snapshot: Option<UsageSnapshot>,
    pub logs: VecDeque<LogEntry>,
    pub selected_session: Option<String>,
    pub connected: bool,
    pub last_error: Option<String>,
    pub status_message: Option<(String, Instant)>,
    pub log_scroll_from_bottom: usize,
    pub log_viewport_height: usize,
    pub log_viewport_width: usize,
    pub tick_count: u64,
    wrapped_logs_cache: RefCell<Option<WrappedLogsCache>>,
}

impl DashboardState {
    pub fn new(config: &Config) -> Self {
        let endpoint_url = config.usage_url();
        let buffer_url = config.buffer_url();
        Self {
            endpoint_url,
            buffer_url,
            snapshot: None,
            logs: VecDeque::new(),
            selected_session: None,
            connected: false,
            last_error: None,
            status_message: None,
            log_scroll_from_bottom: 0,
            log_viewport_height: 10,
            log_viewport_width: 120,
            tick_count: 0,
            wrapped_logs_cache: RefCell::new(None),
        }
    }

    /// Returns the currently selected [`SessionUsageStats`] if a specific session tab is active.
    pub fn active_session(&self) -> Option<&SessionUsageStats> {
        let sid = self.selected_session.as_deref()?;
        self.snapshot
            .as_ref()?
            .sessions
            .iter()
            .find(|s| s.id == sid)
    }

    /// Returns the [`ScopeUsageStats`] for the currently selected session tab, or global stats
    /// when `[All Sessions]` is active.
    pub fn active_stats(&self) -> Option<&ScopeUsageStats> {
        self.active_session()
            .map(|s| &s.stats)
            .or_else(|| self.snapshot.as_ref().map(|s| &s.stats))
    }

    /// Switches to the next session tab (`[All Sessions]` -> `#1` -> `#2` -> ... -> `[All Sessions]`).
    pub fn select_next_session(&mut self) {
        let Some(ref snap) = self.snapshot else {
            self.selected_session = None;
            return;
        };
        if snap.sessions.is_empty() {
            self.selected_session = None;
            return;
        }
        match self.selected_session.as_deref() {
            None => {
                self.selected_session = Some(snap.sessions[0].id.clone());
            }
            Some(cur) => {
                if let Some(pos) = snap.sessions.iter().position(|s| s.id == cur) {
                    if pos + 1 < snap.sessions.len() {
                        self.selected_session = Some(snap.sessions[pos + 1].id.clone());
                    } else {
                        self.selected_session = None;
                    }
                } else {
                    self.selected_session = None;
                }
            }
        }
        self.log_scroll_from_bottom = 0;
    }

    /// Switches to the previous session tab (`[All Sessions]` <- `#1` <- `#2` <- ...).
    pub fn select_prev_session(&mut self) {
        let Some(ref snap) = self.snapshot else {
            self.selected_session = None;
            return;
        };
        if snap.sessions.is_empty() {
            self.selected_session = None;
            return;
        }
        match self.selected_session.as_deref() {
            None => {
                self.selected_session = snap.sessions.last().map(|s| s.id.clone());
            }
            Some(cur) => {
                if let Some(pos) = snap.sessions.iter().position(|s| s.id == cur) {
                    if pos > 0 {
                        self.selected_session = Some(snap.sessions[pos - 1].id.clone());
                    } else {
                        self.selected_session = None;
                    }
                } else {
                    self.selected_session = None;
                }
            }
        }
        self.log_scroll_from_bottom = 0;
    }

    pub fn sorted_models(&self) -> Vec<&ModelUsageStats> {
        let Some(stats) = self.active_stats() else {
            return Vec::new();
        };
        let mut list: Vec<&ModelUsageStats> = stats.models.values().collect();
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

    fn matches_session(entry: &LogEntry, selected_session: Option<&str>) -> bool {
        match selected_session {
            Some(sid) => entry.session_id.as_deref() == Some(sid),
            None => true,
        }
    }

    /// Returns log entries filtered by the active session tab (or all logs when `[All Sessions]` is selected).
    pub fn visible_logs(&self) -> Vec<&LogEntry> {
        self.logs
            .iter()
            .filter(|l| Self::matches_session(l, self.selected_session.as_deref()))
            .collect()
    }

    fn ensure_wrapped_logs_cache(&self, inner_width: usize) {
        let first_id = self.logs.front().map(|l| l.id);
        let last_id = self.logs.back().map(|l| l.id);
        let log_count = self.logs.len();

        {
            let cache_ref = self.wrapped_logs_cache.borrow();
            if let Some(ref cache) = *cache_ref
                && cache.inner_width == inner_width
                && cache.log_count == log_count
                && cache.first_log_id == first_id
                && cache.last_log_id == last_id
                && cache.selected_session == self.selected_session
            {
                return;
            }
        }

        let mut lines = VecDeque::new();
        let mut visible_entry_count = 0usize;
        for entry in &self.logs {
            if Self::matches_session(entry, self.selected_session.as_deref()) {
                visible_entry_count += 1;
                for (sub_idx, line) in format_wrapped_log_entry(entry, inner_width)
                    .into_iter()
                    .enumerate()
                {
                    lines.push_back((entry.id, sub_idx, line));
                }
            }
        }

        *self.wrapped_logs_cache.borrow_mut() = Some(WrappedLogsCache {
            selected_session: self.selected_session.clone(),
            inner_width,
            log_count,
            first_log_id: first_id,
            last_log_id: last_id,
            visible_entry_count,
            lines,
        });
    }

    fn with_wrapped_logs_cache<R>(
        &self,
        inner_width: usize,
        f: impl FnOnce(&WrappedLogsCache) -> R,
    ) -> R {
        self.ensure_wrapped_logs_cache(inner_width);
        let cache_ref = self.wrapped_logs_cache.borrow();
        f(cache_ref.as_ref().expect("wrapped logs cache initialized"))
    }

    /// Returns all visible log entries pre-wrapped into visual terminal [`Line`]s for `inner_width`,
    /// tagged with `(entry_id, subline_index, line)`.
    pub fn visual_log_lines(&self, inner_width: usize) -> Vec<(u64, usize, Line<'static>)> {
        self.with_wrapped_logs_cache(inner_width, |cache| cache.lines.iter().cloned().collect())
    }

    /// Maximum number of visual lines the log panel can scroll up from the bottom
    /// while keeping the viewport full.
    pub fn max_log_scroll(&self) -> usize {
        self.with_wrapped_logs_cache(self.log_viewport_width, |cache| {
            cache
                .lines
                .len()
                .saturating_sub(self.log_viewport_height.max(1))
        })
    }

    pub fn update_viewport_size(&mut self, area: Rect) {
        let chunks = dashboard_chunks(area, self);
        self.log_viewport_height = chunks[3].height.saturating_sub(2) as usize;
        self.log_viewport_width = chunks[3].width.saturating_sub(2) as usize;
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

    /// Applies an incremental [`LogStreamEvent`] received from `GET /logs`.
    pub fn apply_log_event(&mut self, event: LogStreamEvent) {
        match event {
            LogStreamEvent::Snapshot { entries } => self.set_logs(entries),
            LogStreamEvent::Append { entries } => self.append_logs(entries),
        }
    }

    /// Replaces the log buffer while keeping the viewport anchored on the same visual log line when paused.
    pub fn set_logs(&mut self, new_logs: Vec<LogEntry>) {
        let mut deque = VecDeque::from(new_logs);
        while deque.len() > MAX_LOG_ENTRIES {
            deque.pop_front();
        }

        if self.log_scroll_from_bottom > 0 {
            let width = self.log_viewport_width;
            let anchor = self.with_wrapped_logs_cache(width, |cache| {
                if cache.lines.is_empty() {
                    return None;
                }
                let max_prev_scroll = cache
                    .lines
                    .len()
                    .saturating_sub(self.log_viewport_height.max(1));
                let effective_prev_scroll = self.log_scroll_from_bottom.min(max_prev_scroll);
                let prev_end_idx = cache.lines.len().saturating_sub(effective_prev_scroll);
                if prev_end_idx > 0 {
                    cache
                        .lines
                        .get(prev_end_idx - 1)
                        .map(|&(id, sub, _)| (id, sub))
                } else {
                    None
                }
            });

            self.logs = deque;
            *self.wrapped_logs_cache.borrow_mut() = None;

            if let Some((anchor_id, anchor_sub)) = anchor {
                let (found_pos, new_len) = self.with_wrapped_logs_cache(width, |cache| {
                    let pos = cache
                        .lines
                        .iter()
                        .position(|(id, sub, _)| *id == anchor_id && *sub == anchor_sub);
                    (pos, cache.lines.len())
                });
                let max_scroll = new_len.saturating_sub(self.log_viewport_height.max(1));
                if let Some(pos) = found_pos {
                    let new_scroll = new_len.saturating_sub(pos + 1);
                    self.log_scroll_from_bottom = new_scroll.min(max_scroll);
                } else {
                    self.log_scroll_from_bottom = max_scroll;
                }
                return;
            }
        } else {
            self.logs = deque;
            *self.wrapped_logs_cache.borrow_mut() = None;
        }

        let max_scroll = self.max_log_scroll();
        if self.log_scroll_from_bottom > max_scroll {
            self.log_scroll_from_bottom = max_scroll;
        }
    }

    /// Incrementally appends newly received log entries, updating the wrapped-line cache in O(k)
    /// and preserving the viewport scroll anchor when paused.
    pub fn append_logs(&mut self, new_entries: Vec<LogEntry>) {
        if new_entries.is_empty() {
            return;
        }

        // When paused, warm the cache before mutating so we can shift the scroll offset in O(1).
        let prev_scroll = if self.log_scroll_from_bottom > 0 {
            let width = self.log_viewport_width;
            let old_max = self.with_wrapped_logs_cache(width, |cache| {
                cache
                    .lines
                    .len()
                    .saturating_sub(self.log_viewport_height.max(1))
            });
            Some(self.log_scroll_from_bottom.min(old_max))
        } else {
            None
        };

        let old_first_id = self.logs.front().map(|l| l.id);
        let old_last_id = self.logs.back().map(|l| l.id);
        let old_count = self.logs.len();

        let mut added_count = 0usize;
        for entry in new_entries {
            if self.logs.back().is_some_and(|last| entry.id <= last.id) {
                continue;
            }
            self.logs.push_back(entry);
            added_count += 1;
        }

        if added_count == 0 {
            return;
        }

        let mut max_evicted_id: Option<u64> = None;
        let mut evicted_old_count = 0usize;
        let mut evicted_visible_entries = 0usize;
        while self.logs.len() > MAX_LOG_ENTRIES {
            if let Some(evicted) = self.logs.pop_front() {
                max_evicted_id = Some(evicted.id);
                if evicted_old_count < old_count {
                    evicted_old_count += 1;
                    if Self::matches_session(&evicted, self.selected_session.as_deref()) {
                        evicted_visible_entries += 1;
                    }
                }
            }
        }

        let new_first_id = self.logs.front().map(|l| l.id);
        let new_last_id = self.logs.back().map(|l| l.id);
        let new_count = self.logs.len();

        let mut added_visual_lines = 0usize;
        let mut cache_updated = false;
        if let Some(ref mut cache) = *self.wrapped_logs_cache.borrow_mut()
            && cache.selected_session == self.selected_session
            && cache.log_count == old_count
            && cache.first_log_id == old_first_id
            && cache.last_log_id == old_last_id
        {
            if let Some(evicted_id) = max_evicted_id {
                while let Some(&(id, _, _)) = cache.lines.front() {
                    if id <= evicted_id {
                        cache.lines.pop_front();
                    } else {
                        break;
                    }
                }
                cache.visible_entry_count = cache
                    .visible_entry_count
                    .saturating_sub(evicted_visible_entries);
            }

            let tail_start = self.logs.len().saturating_sub(added_count);
            for entry in self.logs.range(tail_start..) {
                if Self::matches_session(entry, self.selected_session.as_deref()) {
                    cache.visible_entry_count += 1;
                    for (sub_idx, line) in format_wrapped_log_entry(entry, cache.inner_width)
                        .into_iter()
                        .enumerate()
                    {
                        cache.lines.push_back((entry.id, sub_idx, line));
                        added_visual_lines += 1;
                    }
                }
            }

            cache.log_count = new_count;
            cache.first_log_id = new_first_id;
            cache.last_log_id = new_last_id;
            cache_updated = true;
        }

        if !cache_updated {
            *self.wrapped_logs_cache.borrow_mut() = None;
        }

        if let Some(effective_prev_scroll) = prev_scroll {
            let max_scroll = self.max_log_scroll();
            self.log_scroll_from_bottom = effective_prev_scroll
                .saturating_add(added_visual_lines)
                .min(max_scroll);
        }
    }

    fn clear_logs_for_session(&mut self, session_id: Option<&str>) {
        if let Some(sid) = session_id {
            self.logs.retain(|l| l.session_id.as_deref() != Some(sid));
        } else {
            self.logs.clear();
        }
        *self.wrapped_logs_cache.borrow_mut() = None;
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

fn dashboard_chunks(area: Rect, state: &DashboardState) -> Rc<[Rect]> {
    let model_rows = state.sorted_models().len().max(1) as u16;
    // 2 border rows + 1 header row + N model rows
    let models_table_height = model_rows.saturating_add(3);

    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),                   // Header bar + Session tab bar
            Constraint::Length(6),                   // 4 Summary cards
            Constraint::Length(models_table_height), // Per-model breakdown table (sized to content)
            Constraint::Fill(1), // Real-time gateway logs (expands to remaining space)
            Constraint::Length(1), // Footer keybindings bar
        ])
        .split(area)
}

/// Renders the complete TUI dashboard frame from [`DashboardState`].
pub fn render_dashboard(frame: &mut Frame, state: &DashboardState) {
    let chunks = dashboard_chunks(frame.area(), state);

    render_header(frame, chunks[0], state);
    render_summary_cards(frame, chunks[1], state);
    render_models_table(frame, chunks[2], state);
    render_logs_panel(frame, chunks[3], state);
    render_footer(frame, chunks[4], state);
}

fn render_header(frame: &mut Frame, area: Rect, state: &DashboardState) {
    let spinner_frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let spinner = spinner_frames[(state.tick_count as usize) % spinner_frames.len()];

    let active_in_flight = state.active_stats().map_or(0, |s| s.active_requests);

    let (status_span, uptime_str, pricing_src, buffer_span, active_span) =
        match (&state.snapshot, state.connected) {
            (Some(snap), true) => {
                let active = if active_in_flight > 0 {
                    Span::styled(
                        format!(" {} {} in-flight ", spinner, active_in_flight),
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::styled(" idle ", Style::default().fg(Color::DarkGray))
                };
                let buffer = if snap.buffer_enabled {
                    Span::styled(
                        "ON",
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::styled("OFF", Style::default().fg(Color::DarkGray))
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
                    buffer,
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
                Span::styled("n/a", Style::default().fg(Color::DarkGray)),
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
        Span::styled(" │ Buffer: ", Style::default().fg(Color::DarkGray)),
        buffer_span,
        Span::styled(" │ Uptime: ", Style::default().fg(Color::DarkGray)),
        Span::styled(uptime_str, Style::default().fg(Color::White)),
        Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
        active_span,
    ]);

    let mut session_spans = vec![Span::styled(
        " Sessions (Tab/←/→): ",
        Style::default().fg(Color::DarkGray),
    )];

    let all_selected = state.selected_session.is_none();
    let total_sessions = state
        .snapshot
        .as_ref()
        .map(|s| s.sessions.len())
        .unwrap_or(0);
    let all_cost = state
        .snapshot
        .as_ref()
        .map(|s| s.totals.total_cost_usd)
        .unwrap_or(0.0);
    let all_label = format!(
        " All Sessions ({}) · {} ",
        total_sessions,
        format_usd(all_cost)
    );
    session_spans.push(Span::styled(
        all_label,
        if all_selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        },
    ));

    if let Some(ref snap) = state.snapshot {
        for sess in &snap.sessions {
            session_spans.push(Span::raw("  "));
            let is_selected = state.selected_session.as_deref() == Some(sess.id.as_str());
            let indicator = if sess.active_requests > 0 {
                spinner
            } else if sess.active {
                "●"
            } else {
                "○"
            };
            let suffix = if !sess.active {
                " [exited]".to_string()
            } else if let Some(pid) = sess.pid {
                format!(" ({})", pid)
            } else {
                String::new()
            };
            let tab_text = format!(
                " {} #{} {}{} · {} ",
                indicator,
                short_session_id(&sess.id),
                sess.label,
                suffix,
                format_usd(sess.totals.total_cost_usd)
            );
            let style = if is_selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if sess.active {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            session_spans.push(Span::styled(tab_text, style));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Antigravity Enterprise Gateway — Live Token & Price Monitor ");

    let paragraph = Paragraph::new(vec![header_line, Line::from(session_spans)]).block(block);
    frame.render_widget(paragraph, area);
}

fn render_metric_card(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    border_color: Color,
    rows: [(&'static str, String, Style); 4],
) {
    let lines: Vec<Line> = rows
        .into_iter()
        .map(|(label, value, style)| {
            Line::from(vec![
                Span::styled(label, Style::default().fg(Color::DarkGray)),
                Span::styled(value, style),
            ])
        })
        .collect();

    let card = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(title),
    );
    frame.render_widget(card, area);
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

    let default_stats = ScopeUsageStats::default();
    let stats = state.active_stats().unwrap_or(&default_stats);
    let totals = &stats.totals;

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

    let bold = |fg: Color| Style::default().fg(fg).add_modifier(Modifier::BOLD);
    let plain = |fg: Color| Style::default().fg(fg);

    render_metric_card(
        frame,
        cards[0],
        " Input Tokens ",
        Color::Blue,
        [
            (
                "Total Input: ",
                format_number(totals.input_tokens),
                bold(Color::White),
            ),
            (
                "Uncached:    ",
                format_number(totals.uncached_input_tokens),
                plain(Color::Cyan),
            ),
            (
                "Uncached $:  ",
                format_usd(totals.uncached_input_cost_usd),
                plain(Color::Yellow),
            ),
            (
                "Total In $:  ",
                format_usd(totals.input_cost_usd),
                bold(Color::Green),
            ),
        ],
    );

    render_metric_card(
        frame,
        cards[1],
        " Cached Input Tokens ",
        Color::Magenta,
        [
            (
                "Cached Tok:  ",
                format_number(totals.cached_input_tokens),
                bold(Color::Magenta),
            ),
            (
                "Cache Hit:   ",
                format!("{:.1}%", cache_hit_pct),
                plain(Color::Cyan),
            ),
            (
                "Cached $:    ",
                format_usd(totals.cached_input_cost_usd),
                plain(Color::Yellow),
            ),
            (
                "Cache Saved: ",
                format_usd(totals.cache_savings_usd),
                bold(Color::Green),
            ),
        ],
    );

    render_metric_card(
        frame,
        cards[2],
        " Output & Thinking Tokens ",
        Color::Yellow,
        [
            (
                "Output Tok:  ",
                format_number(totals.output_tokens),
                bold(Color::White),
            ),
            (
                "Candidates:  ",
                format_number(totals.candidates_tokens),
                plain(Color::Cyan),
            ),
            (
                "Thinking:    ",
                format!(
                    "{} ({:.0}%)",
                    format_number(totals.thoughts_tokens),
                    think_pct
                ),
                plain(Color::LightYellow),
            ),
            (
                "Output $:    ",
                format_usd(totals.output_cost_usd),
                bold(Color::Green),
            ),
        ],
    );

    render_metric_card(
        frame,
        cards[3],
        " Total Price & Reliability ",
        Color::Green,
        [
            (
                "Total Price: ",
                format_usd(totals.total_cost_usd),
                bold(Color::LightGreen),
            ),
            (
                "All Tokens:  ",
                format_number(totals.total_tokens),
                plain(Color::White),
            ),
            (
                "Requests:    ",
                format!(
                    "{} ok / {} err",
                    stats.successful_requests, stats.failed_requests
                ),
                plain(if stats.failed_requests > 0 {
                    Color::LightRed
                } else {
                    Color::Cyan
                }),
            ),
            (
                "Retries:     ",
                format_number(stats.total_retries),
                plain(if stats.total_retries > 0 {
                    Color::Yellow
                } else {
                    Color::DarkGray
                }),
            ),
        ],
    );
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

    let header = Row::new(header_cells).height(1);
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

fn wrap_text_to_width(text: &str, max_width: usize) -> Vec<String> {
    let width = max_width.max(1);
    let mut wrapped = Vec::new();

    for raw_line in text.split('\n') {
        if raw_line.is_empty() {
            wrapped.push(String::new());
            continue;
        }

        let chars: Vec<char> = raw_line.chars().collect();
        let mut start = 0;
        while start < chars.len() {
            let remaining = chars.len() - start;
            if remaining <= width {
                let chunk: String = chars[start..].iter().collect();
                wrapped.push(chunk);
                break;
            }

            let end_limit = start + width;
            let split_at = chars[start..end_limit]
                .iter()
                .rposition(|&c| c == ' ')
                .filter(|&rel_pos| rel_pos > 0)
                .map(|rel_pos| start + rel_pos);

            if let Some(space_idx) = split_at {
                let chunk: String = chars[start..space_idx].iter().collect();
                wrapped.push(chunk);
                start = space_idx + 1;
                while start < chars.len() && chars[start] == ' ' {
                    start += 1;
                }
            } else {
                let chunk: String = chars[start..end_limit].iter().collect();
                wrapped.push(chunk);
                start = end_limit;
            }
        }
    }

    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    wrapped
}

fn format_wrapped_log_entry(entry: &LogEntry, inner_width: usize) -> Vec<Line<'static>> {
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

    let ts_prefix = format!("[{}] ", format_duration_secs(entry.timestamp_secs));
    let lvl_prefix = format!("{} ", level_str);
    let prefix_width = ts_prefix.chars().count() + lvl_prefix.chars().count();
    let msg_width = inner_width.saturating_sub(prefix_width).max(12);
    let indent = " ".repeat(prefix_width);

    let chunks = wrap_text_to_width(&entry.message, msg_width);
    let mut lines = Vec::with_capacity(chunks.len());
    for (idx, chunk) in chunks.into_iter().enumerate() {
        if idx == 0 {
            lines.push(Line::from(vec![
                Span::styled(ts_prefix.clone(), Style::default().fg(Color::DarkGray)),
                Span::styled(lvl_prefix.clone(), level_style),
                Span::styled(chunk, Style::default().fg(Color::White)),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw(indent.clone()),
                Span::styled(chunk, Style::default().fg(Color::White)),
            ]));
        }
    }
    lines
}

fn render_logs_panel(frame: &mut Frame, area: Rect, state: &DashboardState) {
    let inner_width = area.width.saturating_sub(2) as usize;
    let visible_rows = area.height.saturating_sub(2) as usize;
    let (total_entries, effective_scroll, lines) =
        state.with_wrapped_logs_cache(inner_width, |cache| {
            let total_entries = cache.visible_entry_count;
            let total_visual = cache.lines.len();
            let max_scroll = total_visual.saturating_sub(visible_rows);
            let effective_scroll = state.log_scroll_from_bottom.min(max_scroll);
            let end_idx = total_visual.saturating_sub(effective_scroll);
            let start_idx = end_idx.saturating_sub(visible_rows);

            let lines: Vec<Line> = if cache.lines.is_empty() {
                vec![Line::from(Span::styled(
                    "Waiting for gateway logs...",
                    Style::default().fg(Color::DarkGray),
                ))]
            } else {
                cache
                    .lines
                    .range(start_idx..end_idx)
                    .map(|(_, _, line)| line.clone())
                    .collect()
            };
            (total_entries, effective_scroll, lines)
        });

    let title = if effective_scroll > 0 {
        format!(
            " Live Gateway Logs (paused -{} lines, ↑/↓/PgUp/PgDn to scroll) ",
            effective_scroll
        )
    } else {
        format!(
            " Live Gateway Logs ({} entries, ↑/↓/PgUp/PgDn to scroll) ",
            total_entries
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
    let reset_label = if state.selected_session.is_some() {
        " Reset Session  "
    } else {
        " Reset All  "
    };
    let buffer_enabled = state
        .snapshot
        .as_ref()
        .is_some_and(|snap| snap.buffer_enabled);
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
            " Tab / ←/→ ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" Switch Session  "),
        Span::styled(
            " r ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(reset_label),
        Span::styled(
            " b ",
            Style::default()
                .fg(Color::Black)
                .bg(if buffer_enabled {
                    Color::Yellow
                } else {
                    Color::White
                })
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" Toggle Buffer  "),
        Span::styled(
            " ↑/↓ / PgUp/PgDn ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" Scroll Logs  "),
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
    LogEvent(LogStreamEvent),
    Disconnected(String),
}

fn spawn_sse_listener<T, F>(
    client: reqwest::Client,
    url: String,
    read_timeout: Duration,
    report_disconnect: bool,
    map_event: F,
    tx: tokio::sync::mpsc::UnboundedSender<SseUpdate>,
) -> tokio::task::JoinHandle<()>
where
    T: DeserializeOwned + Send + 'static,
    F: Fn(T) -> SseUpdate + Send + 'static,
{
    tokio::spawn(async move {
        let send_disconnect = |reason: String| {
            if report_disconnect {
                tx.send(SseUpdate::Disconnected(reason)).is_ok()
            } else {
                !tx.is_closed()
            }
        };

        loop {
            match client.get(&url).send().await {
                Ok(res) if res.status().is_success() => {
                    let mut stream = res.bytes_stream();
                    let mut buf = Vec::new();
                    let mut stream_err = "SSE stream closed".to_string();

                    loop {
                        match tokio::time::timeout(read_timeout, stream.next()).await {
                            Ok(Some(Ok(chunk))) => {
                                buf.extend_from_slice(&chunk);
                                while let Some(json_str) = drain_sse_data_line(&mut buf) {
                                    if let Ok(parsed) = serde_json::from_str::<T>(&json_str)
                                        && tx.send(map_event(parsed)).is_err()
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

                    if !send_disconnect(stream_err) {
                        return;
                    }
                }
                Ok(res) => {
                    if !send_disconnect(format!("HTTP {}", res.status())) {
                        return;
                    }
                }
                Err(e) => {
                    if !send_disconnect(format!("Connection error: {}", e)) {
                        return;
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
}

fn spawn_usage_sse_listener(
    client: reqwest::Client,
    usage_url: String,
    tx: tokio::sync::mpsc::UnboundedSender<SseUpdate>,
) -> tokio::task::JoinHandle<()> {
    spawn_sse_listener::<UsageSnapshot, _>(
        client,
        usage_url,
        Duration::from_secs(5),
        true,
        |snap| SseUpdate::Snapshot(Box::new(snap)),
        tx,
    )
}

fn spawn_logs_sse_listener(
    client: reqwest::Client,
    logs_url: String,
    tx: tokio::sync::mpsc::UnboundedSender<SseUpdate>,
) -> tokio::task::JoinHandle<()> {
    spawn_sse_listener::<WireLogEvent, _>(
        client,
        logs_url,
        Duration::from_secs(30),
        false,
        |wire| SseUpdate::LogEvent(wire.into()),
        tx,
    )
}

struct TerminalRestoreGuard;

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
    }
}

/// Runs the interactive live TUI usage and cost monitor dashboard over SSE.
pub async fn run_monitor(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let sse_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .tcp_keepalive(Duration::from_secs(15))
        .build()?;
    let cmd_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;

    let mut state = DashboardState::new(&config);
    let logs_url = config.logs_url();
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
                    if let Some(ref sid) = state.selected_session
                        && !snapshot.sessions.iter().any(|s| &s.id == sid)
                    {
                        state.selected_session = None;
                    }
                    state.snapshot = Some(*snapshot);
                    state.connected = true;
                    state.last_error = None;
                }
                SseUpdate::LogEvent(event) => {
                    state.apply_log_event(event);
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
                KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                    state.select_next_session();
                }
                KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                    state.select_prev_session();
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
                    let target_session = state.selected_session.clone();
                    match reset_remote_session_usage(
                        cmd_client,
                        &state.endpoint_url,
                        target_session.as_deref(),
                    )
                    .await
                    {
                        Ok(snap) => {
                            if let Some(ref sid) = state.selected_session
                                && !snap.sessions.iter().any(|s| &s.id == sid)
                            {
                                state.selected_session = None;
                            }
                            state.clear_logs_for_session(target_session.as_deref());
                            state.snapshot = Some(snap);
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
                KeyCode::Char('b') => {
                    match toggle_remote_buffer(cmd_client, &state.buffer_url, None).await {
                        Ok(snap) => {
                            if let Some(ref sid) = state.selected_session
                                && !snap.sessions.iter().any(|s| &s.id == sid)
                            {
                                state.selected_session = None;
                            }
                            let enabled = snap.buffer_enabled;
                            state.snapshot = Some(snap);
                            state.connected = true;
                            state.last_error = None;
                            state.status_message = Some((
                                format!(
                                    "Full SSE buffering {}.",
                                    if enabled { "enabled" } else { "disabled" }
                                ),
                                Instant::now(),
                            ));
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
    use crate::usage::{LogEntry, ModelPricing, ScopeUsageStats, TokenUsageTotals};
    use ratatui::backend::TestBackend;
    use std::collections::BTreeMap;

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
        let mut state = DashboardState::new(&Config::default());
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

        let totals = models["gemini-4-argon"].totals.clone();
        let scope_stats = ScopeUsageStats {
            active_requests: 1,
            total_requests: 5,
            successful_requests: 5,
            failed_requests: 0,
            total_retries: 1,
            totals,
            models,
        };
        state.snapshot = Some(UsageSnapshot {
            uptime_secs: 95,
            stats: scope_stats.clone(),
            pricing_source: "models.dev (live)".to_string(),
            buffer_enabled: false,
            sessions: vec![SessionUsageStats {
                id: "1".to_string(),
                label: "agy-gyro".to_string(),
                pid: Some(48210),
                active: true,
                stats: scope_stats,
            }],
        });
        state.logs = VecDeque::from(vec![LogEntry {
            id: 1,
            timestamp_secs: 90,
            level: "INFO".to_string(),
            target: "agy_gyro::proxy".to_string(),
            message: "Request #5 [gemini-4-argon] status=200".to_string(),
            session_id: Some("1".to_string()),
        }]);

        let backend = TestBackend::new(140, 38);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_dashboard(frame, &state))
            .unwrap();

        let render_to_string = |term: &Terminal<TestBackend>| {
            let buffer = term.backend().buffer();
            let mut rendered = String::new();
            for y in 0..buffer.area.height {
                for x in 0..buffer.area.width {
                    rendered.push_str(buffer[(x, y)].symbol());
                }
                rendered.push('\n');
            }
            rendered
        };

        let rendered = render_to_string(&terminal);

        assert!(rendered.contains("CONNECTED"));
        assert!(rendered.contains("Buffer: OFF"));
        assert!(rendered.contains("Toggle Buffer"));
        assert!(rendered.contains("All Sessions (1)"));
        assert!(rendered.contains("#1 agy-gyro (48210)"));
        assert!(rendered.contains("gemini-4-argon"));
        assert!(rendered.contains("150,000"));
        assert!(rendered.contains("120,000"));
        assert!(rendered.contains("$0.1720"));
        assert!(!rendered.contains("Recent Requests Activity"));
        assert!(rendered.contains("Live Gateway Logs"));
        assert!(rendered.contains("Request #5 [gemini-4-argon] status=200"));

        // Verify Buffer: ON indicator when full buffering is enabled
        state.snapshot.as_mut().unwrap().buffer_enabled = true;
        terminal
            .draw(|frame| render_dashboard(frame, &state))
            .unwrap();
        let rendered_buffered = render_to_string(&terminal);
        assert!(rendered_buffered.contains("Buffer: ON"));

        // Verify session tab navigation
        state.select_next_session();
        assert_eq!(state.selected_session.as_deref(), Some("1"));
        state.select_next_session();
        assert_eq!(state.selected_session, None);
        state.select_prev_session();
        assert_eq!(state.selected_session.as_deref(), Some("1"));
    }

    #[test]
    fn test_log_scrolling_bounds_at_top_and_bottom() {
        let mut state = DashboardState::new(&Config::default());
        state.log_viewport_height = 5;

        // 1. Fewer logs than viewport height: scrolling up should stay at 0
        state.logs = (1..=3)
            .map(|id| LogEntry {
                id,
                timestamp_secs: id,
                level: "INFO".to_string(),
                target: "agy_gyro".to_string(),
                message: format!("Log {}", id),
                session_id: None,
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
                session_id: None,
            })
            .collect();
        state.scroll_logs_up(100);
        assert_eq!(state.log_scroll_from_bottom, 7);

        // Scrolling down 1 line from the very top immediately moves to 6
        state.scroll_logs_down(1);
        assert_eq!(state.log_scroll_from_bottom, 6);

        // Paused viewport stays anchored on the same lines when new logs arrive via Snapshot
        let new_logs: Vec<LogEntry> = (1..=15)
            .map(|id| LogEntry {
                id,
                timestamp_secs: id,
                level: "INFO".to_string(),
                target: "agy_gyro".to_string(),
                message: format!("Log {}", id),
                session_id: None,
            })
            .collect();
        state.set_logs(new_logs);
        assert_eq!(state.log_scroll_from_bottom, 9);

        // Paused viewport also stays anchored when new logs arrive incrementally via Append
        state.apply_log_event(LogStreamEvent::Append {
            entries: (16..=18)
                .map(|id| LogEntry {
                    id,
                    timestamp_secs: id,
                    level: "INFO".to_string(),
                    target: "agy_gyro".to_string(),
                    message: format!("Log {}", id),
                    session_id: None,
                })
                .collect(),
        });
        assert_eq!(state.logs.len(), 18);
        assert_eq!(state.log_scroll_from_bottom, 12);
    }

    #[test]
    fn test_incremental_append_cache_eviction_and_wire_compat() {
        // WireLogEvent supports both tagged LogStreamEvent and legacy bare Vec<LogEntry>
        let snapshot_json = serde_json::to_string(&LogStreamEvent::Snapshot {
            entries: vec![LogEntry {
                id: 1,
                timestamp_secs: 1,
                level: "INFO".to_string(),
                target: "agy_gyro".to_string(),
                message: "hello".to_string(),
                session_id: None,
            }],
        })
        .unwrap();
        let parsed_snap: LogStreamEvent = serde_json::from_str::<WireLogEvent>(&snapshot_json)
            .unwrap()
            .into();
        assert!(matches!(parsed_snap, LogStreamEvent::Snapshot { entries } if entries.len() == 1));

        let append_json = serde_json::to_string(&LogStreamEvent::Append {
            entries: vec![LogEntry {
                id: 2,
                timestamp_secs: 2,
                level: "WARN".to_string(),
                target: "agy_gyro".to_string(),
                message: "world".to_string(),
                session_id: Some("s1".to_string()),
            }],
        })
        .unwrap();
        let parsed_append: LogStreamEvent = serde_json::from_str::<WireLogEvent>(&append_json)
            .unwrap()
            .into();
        assert!(matches!(parsed_append, LogStreamEvent::Append { entries } if entries.len() == 1));

        let legacy_json = serde_json::to_string(&vec![LogEntry {
            id: 9,
            timestamp_secs: 9,
            level: "INFO".to_string(),
            target: "agy_gyro".to_string(),
            message: "legacy".to_string(),
            session_id: None,
        }])
        .unwrap();
        let parsed_legacy: LogStreamEvent = serde_json::from_str::<WireLogEvent>(&legacy_json)
            .unwrap()
            .into();
        assert!(
            matches!(parsed_legacy, LogStreamEvent::Snapshot { entries } if entries[0].id == 9)
        );

        // Verify incremental cache matches a fresh rebuild across session filter + overflow eviction
        let mut state = DashboardState::new(&Config::default());
        state.log_viewport_height = 5;
        state.log_viewport_width = 80;
        state.selected_session = Some("s1".to_string());

        let initial: Vec<LogEntry> = (1..=(MAX_LOG_ENTRIES as u64))
            .map(|id| LogEntry {
                id,
                timestamp_secs: id,
                level: "INFO".to_string(),
                target: "agy_gyro".to_string(),
                message: format!("Entry {}", id),
                session_id: Some(if id % 2 == 0 { "s1" } else { "s2" }.to_string()),
            })
            .collect();
        state.apply_log_event(LogStreamEvent::Snapshot { entries: initial });
        assert_eq!(state.logs.len(), MAX_LOG_ENTRIES);

        // Warm cache, then pause 10 lines up from bottom
        state.scroll_logs_up(10);
        assert_eq!(state.log_scroll_from_bottom, 10);

        // Append 4 entries (2 for s1, 2 for s2) + 1 duplicate ID that should be ignored
        let extra = vec![
            LogEntry {
                id: MAX_LOG_ENTRIES as u64,
                timestamp_secs: 0,
                level: "INFO".to_string(),
                target: "agy_gyro".to_string(),
                message: "duplicate".to_string(),
                session_id: Some("s1".to_string()),
            },
            LogEntry {
                id: MAX_LOG_ENTRIES as u64 + 1,
                timestamp_secs: 1,
                level: "INFO".to_string(),
                target: "agy_gyro".to_string(),
                message: "Extra 1".to_string(),
                session_id: Some("s2".to_string()),
            },
            LogEntry {
                id: MAX_LOG_ENTRIES as u64 + 2,
                timestamp_secs: 2,
                level: "INFO".to_string(),
                target: "agy_gyro".to_string(),
                message: "Extra 2".to_string(),
                session_id: Some("s1".to_string()),
            },
            LogEntry {
                id: MAX_LOG_ENTRIES as u64 + 3,
                timestamp_secs: 3,
                level: "INFO".to_string(),
                target: "agy_gyro".to_string(),
                message: "Extra 3".to_string(),
                session_id: Some("s2".to_string()),
            },
            LogEntry {
                id: MAX_LOG_ENTRIES as u64 + 4,
                timestamp_secs: 4,
                level: "INFO".to_string(),
                target: "agy_gyro".to_string(),
                message: "Extra 4".to_string(),
                session_id: Some("s1".to_string()),
            },
        ];
        state.apply_log_event(LogStreamEvent::Append { entries: extra });

        assert_eq!(state.logs.len(), MAX_LOG_ENTRIES);
        assert_eq!(state.logs.front().unwrap().id, 5);
        assert_eq!(state.logs.back().unwrap().id, MAX_LOG_ENTRIES as u64 + 4);
        // 2 new s1 entries were added at bottom -> paused scroll shifts from 10 to 12
        assert_eq!(state.log_scroll_from_bottom, 12);

        // Incremental cache must match a cold rebuild identically
        let cached_lines = state.visual_log_lines(80);
        let cached_visible_count = state.with_wrapped_logs_cache(80, |c| c.visible_entry_count);
        *state.wrapped_logs_cache.borrow_mut() = None;
        let rebuilt_lines = state.visual_log_lines(80);
        let rebuilt_visible_count = state.with_wrapped_logs_cache(80, |c| c.visible_entry_count);

        assert_eq!(cached_visible_count, rebuilt_visible_count);
        assert_eq!(cached_visible_count, MAX_LOG_ENTRIES / 2);
        assert_eq!(cached_lines.len(), rebuilt_lines.len());
        assert_eq!(
            cached_lines.first().unwrap().0,
            rebuilt_lines.first().unwrap().0
        );
        assert_eq!(
            cached_lines.last().unwrap().0,
            rebuilt_lines.last().unwrap().0
        );
    }

    #[test]
    fn test_log_soft_wrapping_and_hanging_indent() {
        let entry = LogEntry {
            id: 1,
            timestamp_secs: 5,
            level: "INFO".to_string(),
            target: "agy_gyro".to_string(),
            message: "First segment of a long message that wraps onto a second visual row\nExplicit newline row".to_string(),
            session_id: None,
        };

        // Prefix "[00:00:05] INFO  " is 17 columns; width 52 leaves 35 columns for message text
        let wrapped = format_wrapped_log_entry(&entry, 52);
        assert!(wrapped.len() >= 3);

        let first_text: String = wrapped[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(first_text.starts_with("[00:00:05] INFO  First segment"));

        // Continuation lines have 17-space hanging indent
        let second_text: String = wrapped[1]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(second_text.starts_with(&" ".repeat(17)));

        let last_text: String = wrapped
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(last_text, format!("{}Explicit newline row", " ".repeat(17)));
    }
}

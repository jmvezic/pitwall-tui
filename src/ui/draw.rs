//! Rendering. The timing tower takes the left 60%; the track status box
//! and race control stack on the right; the lap line runs along the bottom.

use chrono::{DateTime, Local, TimeDelta, Utc};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table, TableState};

use super::strip::{self, StripCtx};
use super::{App, STALE_AFTER, Source, View, WIDE, style};
use crate::config::SignIn;
use crate::feed::ConnStatus;
use crate::state::derive::{
    Benchmark, DriverFlag, Penalty, PushRow, QualiInfo, RcmKind, Reading, RunState, SectorFlag,
    SessionKind, Tag, TimeKind, TowerRow, TrackState, Weather, compass,
};
use crate::state::laps::{TREND_DEADBAND, TrendMode};
use crate::state::quali::SectorSnap;

/// Data older than this (seconds) turns the LIVE indicator yellow.
const LIVE_FRESH: u64 = 5;
/// Race control messages younger than this get a highlighted background.
const RC_NEW: TimeDelta = TimeDelta::seconds(10);
/// Under this terminal width the GAP and LAST columns are dropped.
const NARROW: u16 = 96;
/// The full tower fits in this many columns.
const TOWER_MIN: u16 = 96;
/// Lap line height: three label lanes, the line, labels, the pit lane.
const STRIP_H: u16 = 8;
/// Weather box width, left of the lap line.
const WEATHER_W: u16 = 25;
/// How long the row of a driver who'd now have the title flashes, before
/// it stays gold.
pub const TITLE_FLASH: TimeDelta = TimeDelta::seconds(5);
/// How long the row of a driver just given a penalty flashes red.
const PENALTY_FLASH: TimeDelta = TimeDelta::seconds(5);

pub fn draw(f: &mut Frame, app: &App, blink: bool) {
    let area = f.area();
    let strip_h = if app.show_strip { STRIP_H } else { 0 };
    let [banner, body, strip_area, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(strip_h),
        Constraint::Length(1),
    ])
    .areas(area);
    draw_banner(f, banner, app, blink);
    draw_footer(f, footer, app);
    if app.show_strip {
        match &app.state.views.weather {
            Some(w) if area.width >= NARROW => {
                let [weather, strip] =
                    Layout::horizontal([Constraint::Length(WEATHER_W), Constraint::Min(0)])
                        .areas(strip_area);
                draw_weather(f, weather, w);
                draw_strip(f, strip, app, blink);
            }
            _ => draw_strip(f, strip_area, app, blink),
        }
    }

    let wide = area.width >= WIDE;
    match app.view {
        View::RaceControl => draw_title_and_rc(f, body, app),
        View::Main if wide => {
            let tower_w = (area.width * 6 / 10).max(TOWER_MIN);
            let [left, right] =
                Layout::horizontal([Constraint::Length(tower_w), Constraint::Min(0)]).areas(body);
            draw_tower(f, left, app, false, blink);
            let status_h = status_height(app, true, right.width).min(right.height / 2);
            let [top, stopped, rest] = Layout::vertical([
                Constraint::Length(status_h),
                Constraint::Length(stopped_height(app)),
                Constraint::Min(0),
            ])
            .areas(right);
            draw_track_status(f, top, app, blink, true);
            if stopped.height > 0 {
                draw_stopped(f, stopped, app);
            }
            draw_title_and_rc(f, rest, app);
        }
        View::Main => {
            let status_h = status_height(app, false, body.width).min(body.height / 3);
            let [top, stopped, bottom] = Layout::vertical([
                Constraint::Length(status_h),
                Constraint::Length(stopped_height(app)),
                Constraint::Min(0),
            ])
            .areas(body);
            draw_track_status(f, top, app, blink, false);
            if stopped.height > 0 {
                draw_stopped(f, stopped, app);
            }
            draw_tower(f, bottom, app, area.width < NARROW, blink);
        }
    }
}

pub(super) fn panel(title: &str) -> Block<'_> {
    Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style::dim())
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(Color::Gray),
        ))
}

fn draw_banner(f: &mut Frame, area: Rect, app: &App, blink: bool) {
    let v = &app.state.views;
    let s = &v.session;
    let (text, fg, bg) = style::banner(&v.track, blink);
    let mut left = vec![
        Span::styled(
            format!(" {text:^25} "),
            Style::new().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            if s.title.is_empty() {
                "waiting for session…".to_owned()
            } else {
                s.title.clone()
            },
            Style::new().add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(lap) = s.lap {
        let total = s.total_laps.map(|t| format!("/{t}")).unwrap_or_default();
        left.push(Span::raw(format!("   Lap {lap}{total}")));
    }
    if let Some(t) = s.race_time_at(app.now()) {
        left.push(Span::raw(format!(" · {}", fmt_hms(t))));
    }
    if let Some(c) = s.clock {
        left.push(Span::raw(format!(
            "   {} left",
            fmt_hms(c.remaining_at(app.now()))
        )));
    }
    if !s.status.is_empty() && s.status != "Started" {
        left.push(Span::styled(format!("   {}", s.status), style::dim()));
    }

    let right = match &app.source {
        Source::Live(_) => {
            let since = app.last_msg.map(|t| t.elapsed().as_secs());
            match (&app.conn, app.is_stale()) {
                (Some(ConnStatus::Disconnected(why)), _) => Span::styled(
                    format!(
                        "RECONNECTING ({}) ",
                        why.chars().take(40).collect::<String>()
                    ),
                    Style::new().fg(style::red()),
                ),
                (Some(ConnStatus::Connecting) | None, _) => {
                    Span::styled("CONNECTING… ", Style::new().fg(style::yellow()))
                }
                (_, true) if since.is_some() => Span::styled(
                    format!("STALE {}s ", since.unwrap_or(STALE_AFTER.as_secs())),
                    Style::new().fg(style::red()).add_modifier(Modifier::BOLD),
                ),
                (_, true) => Span::styled("WAITING FOR DATA ", Style::new().fg(style::yellow())),
                // Seconds since the last message: a heartbeat for the feed.
                _ => {
                    let since = since.unwrap_or(0);
                    let colour = if since < LIVE_FRESH {
                        style::green()
                    } else {
                        style::yellow()
                    };
                    let delay = match app.delay() {
                        Some(d) if !d.is_zero() => format!(" · delay {}s", d.as_secs()),
                        _ => String::new(),
                    };
                    Span::styled(
                        format!("● LIVE{delay} · {since}s ago "),
                        Style::new().fg(colour),
                    )
                }
            }
        }
        Source::Replay(_) => match &app.replay {
            Some(st) => {
                let icon = if st.paused { "⏸" } else { "▶" };
                let pos = TimeDelta::from_std(st.current()).unwrap_or_default();
                let end = TimeDelta::from_std(st.end).unwrap_or_default();
                Span::styled(
                    format!(
                        "REPLAY {}× {icon} {} / {} ",
                        st.speed,
                        fmt_hms(pos),
                        fmt_hms(end)
                    ),
                    Style::new().fg(style::blue()),
                )
            }
            None => Span::styled("LOADING… ", Style::new().fg(style::yellow())),
        },
    };

    let right = Line::from(right);
    let [left_area, right_area] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(right.width() as u16 + 1),
    ])
    .areas(area);
    f.render_widget(Paragraph::new(Line::from(left)), left_area);
    f.render_widget(
        Paragraph::new(right).alignment(Alignment::Right),
        right_area,
    );
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let trend = match app.trend_mode {
        TrendMode::Avg3 => "avg3",
        TrendMode::Last => "last",
    };
    let mut keys = format!(
        " q quit  ↑↓ select  t trend:{trend}  f filter{}  m lap line  r race control",
        if app.rc_filter { ":on" } else { "" }
    );
    if matches!(app.source, Source::Live(_)) {
        keys.push_str("  x reset  s sync at lights out  v sync to video time  +/- delay");
    }
    if app.token_file.is_some() && app.sign_in.is_some_and(|s| s != SignIn::Valid) {
        keys.push_str("  l sign in");
    }
    if let Some(text) = &app.video_time {
        let prompt = Line::from(vec![
            Span::styled(
                format!(" {} (e.g. 5:04): ", app.video_time_prompt()),
                Style::new().fg(style::yellow()),
            ),
            Span::styled(
                format!("{text}▏"),
                Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
            Span::styled("   Enter sync · Esc cancel", style::dim()),
        ]);
        f.render_widget(Paragraph::new(prompt), area);
        return;
    }
    if matches!(app.source, Source::Replay(_)) {
        keys.push_str("  space pause  +/- speed  ←/→ 10s  PgUp/PgDn 30s");
    }
    f.render_widget(Paragraph::new(Span::styled(keys, style::dim())), area);
    if let Some(n) = &app.notice {
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("{n} "),
                Style::new().fg(style::yellow()),
            ))
            .alignment(Alignment::Right),
            area,
        );
    }
}

/// Width of one sector cell plus the gap after it.
const CELL_W: u16 = 5;

/// Every marshal sector of the circuit: from circuit data, or up to the
/// highest number race control has mentioned.
fn marshal_sectors(app: &App) -> Vec<u32> {
    if let Some(nums) = app
        .track_line
        .as_ref()
        .map(|l| l.marshal_numbers())
        .filter(|n| !n.is_empty())
    {
        return nums;
    }
    let v = &app.state.views;
    let max = v
        .sector_flags
        .keys()
        .copied()
        .max()
        .unwrap_or(0)
        .max(v.max_sector_seen);
    (1..=max).collect()
}

fn grid_rows(sectors: usize, width: u16) -> u16 {
    let per_row = ((width.saturating_sub(2) + 1) / CELL_W).max(1) as usize;
    sectors.div_ceil(per_row) as u16
}

/// Border, status bar, a gap, the sector grid.
fn status_height(app: &App, tall: bool, width: u16) -> u16 {
    let bar = if tall { 3 } else { 1 };
    let n = marshal_sectors(app).len();
    let grid = if n == 0 { 1 } else { grid_rows(n, width) };
    2 + bar + 1 + grid
}

/// The track status in a coloured bar, then every marshal sector as a cell
/// coloured by its flag.
fn draw_track_status(f: &mut Frame, area: Rect, app: &App, blink: bool, tall: bool) {
    let v = &app.state.views;
    let (text, fg, bg) = style::banner(&v.track, blink);
    let (_, _, border) = style::banner(&v.track, false);
    let block = panel("Track status").border_style(Style::new().fg(border));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let bar_h = if tall { 3 } else { 1 }.min(inner.height);
    let [bar, _, rest] = Layout::vertical([
        Constraint::Length(bar_h),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);
    let bar_style = Style::new().fg(fg).bg(bg).add_modifier(Modifier::BOLD);
    let mut bar_lines = vec![Line::from(""); bar_h as usize];
    bar_lines[bar_h as usize / 2] = Line::from(text);
    f.render_widget(
        Paragraph::new(bar_lines)
            .style(bar_style)
            .alignment(Alignment::Center),
        bar,
    );

    let sectors = marshal_sectors(app);
    if sectors.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(" Sector map unavailable", style::dim())),
            rest,
        );
        return;
    }
    let per_row = ((inner.width + 1) / CELL_W).max(1) as usize;
    let lines: Vec<Line> = sectors
        .chunks(per_row)
        .map(|row| {
            let mut spans = Vec::new();
            for (i, num) in row.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::raw(" "));
                }
                let st =
                    sector_cell_style(v.track == TrackState::Red, v.sector_flags.get(num), blink);
                spans.push(Span::styled(format!("{num:^4}"), st));
            }
            Line::from(spans)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), rest);
}

/// Green when clear, yellow, flashing yellow for a double yellow; all red
/// under a red flag.
fn sector_cell_style(red_flag: bool, flag: Option<&SectorFlag>, blink: bool) -> Style {
    let clear = Style::new().fg(Color::White).bg(style::rgb(30, 110, 55));
    match (red_flag, flag) {
        (true, _) => Style::new().fg(Color::White).bg(style::red()),
        (false, None) => clear,
        (false, Some(fl)) => match style::sector_flag(*fl, blink) {
            Some(c) => Style::new()
                .fg(Color::Black)
                .bg(c)
                .add_modifier(Modifier::BOLD),
            None => Style::new()
                .fg(style::yellow())
                .bg(style::flash_off())
                .add_modifier(Modifier::BOLD),
        },
    }
}

/// Cars stopped on track; only shown when there are any.
fn stopped_height(app: &App) -> u16 {
    match app.state.views.stopped.len() {
        0 => 0,
        n => n as u16 + 2,
    }
}

fn draw_stopped(f: &mut Frame, area: Rect, app: &App) {
    let now = app.now();
    let lines: Vec<Line> = app
        .state
        .views
        .stopped
        .iter()
        .map(|s| {
            let ago = (now - s.since).max(TimeDelta::zero());
            Line::from(vec![
                Span::styled(
                    format!(" {} {} ", s.num, s.tla),
                    Style::new()
                        .fg(Color::White)
                        .bg(style::red())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  lap {} · stopped {} ago", s.lap, fmt_ms(ago)),
                    Style::new().fg(style::red()).add_modifier(Modifier::BOLD),
                ),
            ])
        })
        .collect();
    let block = panel("Stopped on track").border_style(Style::new().fg(style::red()));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_strip(f: &mut Frame, area: Rect, app: &App, blink: bool) {
    let ctx = StripCtx {
        line: app.track_line.as_ref(),
        state: &app.state,
        selected: app.selected.as_deref(),
        blink,
    };
    let mut title = "Lap".to_owned();
    if let Some(t) = app
        .state
        .views
        .session
        .title
        .split(" · ")
        .next()
        .filter(|t| !t.is_empty())
    {
        title = format!("Lap · {t}");
    }
    if !strip::uses_positions(&ctx) {
        title.push_str(" · cars placed by mini-sector");
    }
    if app
        .track_line
        .as_ref()
        .is_some_and(|l| !l.has_marshal_sectors())
    {
        title.push_str(" · no marshal sector map");
    }
    strip::draw(f, area, panel(&title), &ctx);
}

/// Conditions with the change since the previous sample. Rain turns the
/// whole box blue.
fn draw_weather(f: &mut Frame, area: Rect, w: &Weather) {
    let row = |label: &str, value: String, r: Reading| {
        Line::from(vec![
            Span::styled(format!("{label:<6}"), style::dim()),
            Span::raw(format!("{value:>7}")),
            delta_span(r.delta),
        ])
    };
    let wind_dir = w
        .wind_dir
        .map(|d| format!(" {}", compass(d)))
        .unwrap_or_default();
    let (status, bg) = if w.rain {
        ("WET", Some(style::wet()))
    } else {
        ("DRY", None)
    };
    let mut wind = row("Wind", format!("{:.1}m/s", w.wind.value), w.wind);
    wind.push_span(Span::raw(wind_dir));
    let lines = vec![
        Line::from(Span::styled(
            status,
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        )),
        row("Air", format!("{:.1}°C", w.air.value), w.air),
        row("Track", format!("{:.1}°C", w.track.value), w.track),
        row("Humid", format!("{:.1}%", w.humidity.value), w.humidity),
        wind,
    ];
    let mut block = panel("Weather");
    let mut para_style = Style::new();
    if let Some(bg) = bg {
        block = block.style(Style::new().bg(bg));
        para_style = para_style.bg(bg).fg(Color::White);
    }
    f.render_widget(Paragraph::new(lines).style(para_style).block(block), area);
}

/// ` ▲0.2`, ` ▼0.2`, or ` =` when unchanged to one decimal; blank before
/// a second sample.
fn delta_span(delta: Option<f64>) -> Span<'static> {
    match delta {
        None => Span::raw(""),
        Some(d) if (d * 10.0).round() == 0.0 => Span::styled(" =", style::dim()),
        Some(d) => Span::styled(
            format!(" {}{:.1}", if d > 0.0 { '▲' } else { '▼' }, d.abs()),
            Style::new().fg(Color::Gray),
        ),
    }
}

fn draw_tower(f: &mut Frame, area: Rect, app: &App, narrow: bool, blink: bool) {
    if app.state.views.session.kind.timed() {
        return draw_quali(f, area, app, narrow);
    }
    let v = &app.state.views;
    let race = v.session.kind == SessionKind::Race;

    let mut header = vec![
        "P", "DRV", "INT", "GAP", "LAP", "LAST", "BEST", "PIT", "TYRE",
    ];
    let mut widths = vec![2, 4, 7, 8, 3, 8, 8, 3, 11];
    if race {
        header.push("TREND");
        widths.push(9);
        header.push("PACE");
        widths.push(6);
    }
    header.push("");
    widths.push(3);
    header.push("PENALTY");
    widths.push(10);
    if narrow {
        // Drop GAP and LAST.
        for i in [5, 3] {
            header.remove(i);
            widths.remove(i);
        }
    }

    let stopped: std::collections::HashSet<&str> =
        v.stopped.iter().map(|s| s.num.as_str()).collect();
    let rows: Vec<Row> = v
        .tower
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let mut cells: Vec<Line<'static>> = vec![
                Line::from(Span::styled(
                    format!("{:>2}", r.pos),
                    Style::new().add_modifier(Modifier::BOLD),
                )),
                Line::from(vec![
                    Span::styled("▌", Style::new().fg(style::team(r.colour))),
                    Span::styled(
                        r.tla.clone(),
                        Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
                    ),
                ]),
                interval_cell(r),
                Line::from(Span::styled(r.gap.clone(), Style::new().fg(Color::Gray))),
                Line::from(format!("{:>3}", r.laps)),
                Line::from(Span::styled(r.last.clone(), style::laptime(r.last_kind))),
                Line::from(Span::styled(r.best.clone(), style::laptime(r.best_kind))),
                Line::from(format!("{:>3}", r.pits)),
                tyre_cell(r),
            ];
            if race {
                cells.push(trend_cell(r));
                cells.push(pace_cell(r));
            }
            cells.push(tag_cell(r.tag));
            cells.push(penalty_cell(
                v.driver_flags.get(&r.num).copied(),
                v.penalties.get(&r.num).map_or(&[], Vec::as_slice),
            ));
            if narrow {
                cells.remove(5);
                cells.remove(3);
            }
            let retired = r.tag == Some(Tag::Ret);
            if retired {
                // Greyed out, team colour and highlights included.
                for span in cells.iter_mut().flat_map(|l| l.spans.iter_mut()) {
                    span.style = style::retired();
                }
            }
            // Every other row is shaded so a row is easy to follow across.
            let base = if i % 2 == 1 {
                Style::new().bg(style::row_stripe())
            } else {
                Style::new()
            };
            let penalty = penalty_flashing(app, &r.num) && !blink;
            let title = title_bg(app, &r.num, blink);
            if penalty || title.is_some() {
                // Black on red or gold, whatever each cell's own colour.
                for span in cells.iter_mut().flat_map(|l| l.spans.iter_mut()) {
                    span.style = span.style.fg(Color::Black);
                }
            }
            let style = if penalty {
                Style::new().fg(Color::Black).bg(style::red())
            } else if stopped.contains(r.num.as_str()) {
                Style::new().fg(Color::White).bg(style::stopped_row())
            } else if let Some(bg) = title {
                Style::new().fg(Color::Black).bg(bg)
            } else if r.tag == Some(Tag::Pit) {
                Style::new().fg(Color::White).bg(style::pit_row())
            } else if retired {
                base.patch(style::retired())
            } else if r.out {
                base.patch(style::dim())
            } else {
                base
            };
            Row::new(cells.into_iter().map(Cell::from)).style(style)
        })
        .collect();

    let selected = app
        .selected
        .as_ref()
        .and_then(|s| v.tower.iter().position(|r| &r.num == s));
    let mut ts = TableState::default().with_selected(selected);
    let table = Table::new(rows, widths.iter().map(|&w| Constraint::Length(w)))
        .header(Row::new(header).style(style::dim()))
        .column_spacing(1)
        .row_highlight_style(Style::new().bg(style::rgb(45, 45, 70)))
        .block(panel("Timing"));
    f.render_stateful_widget(table, area, &mut ts);
}

/// Qualifying: the standings in this part above the cars on a push lap.
fn draw_quali(f: &mut Frame, area: Rect, app: &App, narrow: bool) {
    let v = &app.state.views;
    let rows: Vec<(&TowerRow, &QualiInfo)> = v
        .tower
        .iter()
        .filter(|r| !r.out)
        .filter_map(|r| Some((r, r.quali.as_ref()?)))
        .collect();
    let drop_line = v.advance.is_some_and(|n| n > 0 && n < rows.len());
    // Header and borders, and room for at least a few push laps.
    let standings_h = rows.len() as u16 + 3 + u16::from(drop_line);
    let push_h = area
        .height
        .saturating_sub(standings_h)
        .max(PUSH_MIN_H.min(area.height / 2));
    let [standings, push] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(push_h)]).areas(area);
    draw_standings(f, standings, app, &rows, narrow);
    draw_push_laps(f, push, app);
}

/// "Qualifying" or "Practice", for box titles.
fn session_name(app: &App) -> &'static str {
    match app.state.views.session.kind {
        SessionKind::Practice => "Practice",
        _ => "Qualifying",
    }
}

/// Height of the push lap box when the standings would leave less.
const PUSH_MIN_H: u16 = 7;
/// How long a new leader's row flashes.
const LEADER_FLASH: TimeDelta = TimeDelta::seconds(4);
/// How long the row of a car that moved up with a new time flashes.
const GAIN_FLASH: TimeDelta = TimeDelta::seconds(2);
/// How long a push lap stays in its box after the line.
const PUSH_LINGER: TimeDelta = TimeDelta::seconds(5);

/// Drivers still in this part with their best lap, and a line above the
/// drop zone.
fn draw_standings(
    f: &mut Frame,
    area: Rect,
    app: &App,
    rows: &[(&TowerRow, &QualiInfo)],
    narrow: bool,
) {
    let v = &app.state.views;
    let now = app.now();
    let stopped: std::collections::HashSet<&str> =
        v.stopped.iter().map(|s| s.num.as_str()).collect();
    let mut header = vec!["P", "DRV", "TYRE", "BEST", "GAP", "S1", "S2", "S3", ""];
    let mut widths = vec![2, 4, 11, 8, 7, 6, 6, 6, 9];
    if narrow {
        for _ in 0..3 {
            header.remove(5);
            widths.remove(5);
        }
    }

    let mut table_rows = Vec::new();
    let mut selected = None;
    for (i, (r, q)) in rows.iter().enumerate() {
        if v.advance == Some(i) && i > 0 {
            table_rows.push(drop_zone_row(&widths));
        }
        if app.selected.as_deref() == Some(r.num.as_str()) {
            selected = Some(table_rows.len());
        }
        let danger = v.advance.is_some_and(|n| i >= n);
        let mut cells: Vec<Line<'static>> = vec![
            Line::from(Span::styled(
                format!("{:>2}", r.pos),
                Style::new()
                    .fg(if danger { style::red() } else { Color::White })
                    .add_modifier(Modifier::BOLD),
            )),
            driver_cell(r.colour, &r.tla),
            tyre_cell(r),
            Line::from(Span::styled(
                q.best.clone(),
                Style::new().fg(if i == 0 {
                    style::purple()
                } else {
                    Color::White
                }),
            )),
            Line::from(Span::styled(q.gap.clone(), Style::new().fg(Color::Gray))),
        ];
        if !narrow {
            for s in 0..3 {
                cells.push(match q.sectors.get(s) {
                    Some(s) => Line::from(Span::styled(
                        format!("{:>6}", s.value),
                        style::laptime(s.kind),
                    )),
                    None => Line::from(""),
                });
            }
        }
        let stopped = stopped.contains(r.num.as_str());
        cells.push(if stopped {
            Line::from(Span::styled(
                "STOPPED",
                Style::new().add_modifier(Modifier::BOLD),
            ))
        } else {
            run_cell(q, now)
        });
        let since = |t: Option<DateTime<Utc>>, d: TimeDelta| t.is_some_and(|t| now - t < d);
        let leader = app.state.quali.new_leader();
        let flash = if stopped {
            None
        } else if leader.is_some_and(|(n, _)| n == r.num)
            && since(leader.map(|l| l.1), LEADER_FLASH)
        {
            Some(style::new_leader())
        } else if since(app.state.quali.gained(&r.num), GAIN_FLASH) {
            Some(style::green())
        } else {
            None
        };
        if flash.is_some() {
            // Black on the flash colour, whatever each cell's own colour.
            for line in &mut cells {
                for span in &mut line.spans {
                    span.style = span.style.fg(Color::Black);
                }
            }
        }
        let base = if stopped {
            Style::new().fg(Color::White).bg(style::stopped_row())
        } else if let Some(bg) = flash {
            Style::new().fg(Color::Black).bg(bg)
        } else if i % 2 == 1 {
            Style::new().bg(style::row_stripe())
        } else {
            Style::new()
        };
        table_rows.push(Row::new(cells.into_iter().map(Cell::from)).style(base));
    }

    let title = format!("{} - standings", session_name(app));
    let mut ts = TableState::default().with_selected(selected);
    let table = Table::new(table_rows, widths.iter().map(|&w| Constraint::Length(w)))
        .header(Row::new(header).style(style::dim()))
        .column_spacing(1)
        .row_highlight_style(Style::new().bg(style::rgb(45, 45, 70)))
        .block(panel(&title));
    f.render_stateful_widget(table, area, &mut ts);
}

/// Cars on a push lap, bar stopped ones, the first to finish on top: each sector coloured
/// once done, and how the lap compares with P1 and the last car through.
fn draw_push_laps(f: &mut Frame, area: Rect, app: &App) {
    let v = &app.state.views;
    let now = app.now();
    let laps: Vec<&PushRow> = v
        .pushing
        .iter()
        .filter(|p| {
            p.end
                .as_ref()
                .is_none_or(|(end, ..)| now - *end < PUSH_LINGER)
        })
        .filter(|p| !v.stopped.iter().any(|s| s.num == p.num))
        .collect();
    let title = format!("{} - push lap", session_name(app));
    let block = panel(&title);
    if laps.is_empty() {
        let p = Paragraph::new(Span::styled("Nobody on a push lap", style::dim())).block(block);
        f.render_widget(p, area);
        return;
    }

    let minis = laps
        .iter()
        .map(|p| p.sectors.iter().map(|s| s.segments.len()).sum::<usize>())
        .max()
        .unwrap_or(0);
    let header = vec![
        "P",
        "DRV",
        "S1",
        "S2",
        "S3",
        "PACE",
        "LAP",
        "BENCHMARK",
        "PRED",
    ];
    let widths = vec![2, 4, 8, 8, 8, (minis as u16 + 2).max(4), 8, 13, 4];
    let mut selected = None;
    let rows: Vec<Row> = laps
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if app.selected.as_deref() == Some(p.num.as_str()) {
                selected = Some(i);
            }
            // The sector being driven, once the lap's under way.
            let current = p.end.is_none().then(|| {
                p.sectors
                    .iter()
                    .position(|s| s.value.is_empty())
                    .unwrap_or(p.sectors.len())
            });
            let mut cells = vec![
                Line::from(Span::styled(
                    format!("{:>2}", p.pos),
                    Style::new().fg(Color::Gray),
                )),
                driver_cell(p.colour, &p.tla),
            ];
            for s in 0..3 {
                cells.push(sector_block(p.sectors.get(s), current == Some(s)));
            }
            cells.push(mini_sector_cell(&p.sectors));
            cells.push(match &p.end {
                Some((_, time, kind)) => Line::from(Span::styled(
                    time.clone(),
                    style::laptime(*kind).add_modifier(Modifier::BOLD),
                )),
                None => Line::from(Span::styled(
                    format!("{:>8}", fmt_ms(now - p.start)),
                    Style::new().fg(Color::Gray),
                )),
            });
            cells.push(benchmark_cell(p.benchmark));
            cells.push(match p.pred {
                Some(n) => Line::from(Span::styled(
                    format!("P{n}"),
                    Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
                )),
                None => Line::from(""),
            });
            let base = if i % 2 == 1 {
                Style::new().bg(style::row_stripe())
            } else {
                Style::new()
            };
            Row::new(cells.into_iter().map(Cell::from)).style(base)
        })
        .collect();

    let mut ts = TableState::default().with_selected(selected);
    let table = Table::new(rows, widths.iter().map(|&w| Constraint::Length(w)))
        .header(Row::new(header).style(style::dim()))
        .column_spacing(1)
        .row_highlight_style(Style::new().bg(style::rgb(45, 45, 70)))
        .block(block);
    f.render_stateful_widget(table, area, &mut ts);
}

fn driver_cell(colour: Option<(u8, u8, u8)>, tla: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("▌", Style::new().fg(style::team(colour))),
        Span::styled(
            tla.to_owned(),
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
    ])
}

/// A sector as a coloured block with its time; grey until done, lighter
/// while being driven.
fn sector_block(s: Option<&SectorSnap>, current: bool) -> Line<'static> {
    let done = s.filter(|s| !s.value.is_empty());
    let (text, st) = match done {
        Some(s) => {
            let bg = match s.kind {
                TimeKind::Overall => style::purple(),
                TimeKind::Personal => style::green(),
                TimeKind::Normal => style::yellow(),
            };
            (
                format!("{:^8}", s.value),
                Style::new()
                    .fg(Color::Black)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            )
        }
        None => (
            " ".repeat(8),
            Style::new().bg(if current {
                style::rgb(80, 80, 80)
            } else {
                style::rgb(40, 40, 40)
            }),
        ),
    };
    Line::from(Span::styled(text, st))
}

/// The lap's mini-sectors in brackets, a dot each: green for a personal
/// best, white for slower than that (on a push lap usually still ahead of
/// the best lap), yellow when F1 gives no colour (usually losing time),
/// blue in the pit lane, blank ahead of the car.
fn mini_sector_cell(sectors: &[SectorSnap]) -> Line<'static> {
    let mut spans = vec![Span::styled("[", style::dim())];
    for &g in sectors.iter().flat_map(|s| &s.segments) {
        let colour = match g {
            0 => None,
            s if s & 16 != 0 => Some(style::blue()),
            s => Some(match s & 7 {
                3 => style::green(),
                1 => Color::White,
                _ => style::yellow(),
            }),
        };
        spans.push(match colour {
            Some(c) => Span::styled("●", Style::new().fg(c)),
            None => Span::raw(" "),
        });
    }
    spans.push(Span::styled("]", style::dim()));
    Line::from(spans)
}

/// `P1  -0.123` or `CUT +0.456`: to P1 purple when faster, grey when not;
/// to the cut green when through, red when not.
fn benchmark_cell(b: Option<(Benchmark, f64)>) -> Line<'static> {
    let Some((kind, d)) = b else {
        return Line::from("");
    };
    let (label, ahead, behind) = match kind {
        Benchmark::P1 => ("P1", style::purple(), Color::Gray),
        Benchmark::Cut => ("CUT", style::green(), style::red()),
    };
    Line::from(vec![
        Span::styled(format!("{label:<3} "), style::dim()),
        Span::styled(
            format!("{d:>+7.3}"),
            Style::new().fg(if d <= 0.0 { ahead } else { behind }),
        ),
    ])
}

/// A dashed red rule across the table, labelled in the last column.
fn drop_zone_row(widths: &[u16]) -> Row<'static> {
    let rule = Style::new().fg(style::red());
    let label_col = widths.len() - 1;
    Row::new(widths.iter().enumerate().map(|(i, &w)| {
        let w = w as usize;
        let text = if i == label_col {
            format!("{:╌<w$}", "DROP ZONE")
        } else {
            "╌".repeat(w)
        };
        Cell::from(Span::styled(text, rule))
    }))
}

/// `PUSH 0:41`, `OUT 1:12`, `COOL`, `PIT`, `STOP` or `FIN`.
fn run_cell(q: &QualiInfo, now: DateTime<Utc>) -> Line<'static> {
    let clock = || {
        q.lap_start
            .map(|t| format!(" {}", fmt_ms(now - t)))
            .unwrap_or_default()
    };
    let (text, st) = match q.run {
        RunState::Push => (
            format!("PUSH{}", clock()),
            Style::new().fg(style::green()).add_modifier(Modifier::BOLD),
        ),
        RunState::Out => (format!("OUT{}", clock()), Style::new().fg(Color::Gray)),
        RunState::Cool => ("COOL".to_owned(), style::dim()),
        RunState::Pit => ("PIT".to_owned(), Style::new().fg(style::blue())),
        RunState::Stopped => ("STOP".to_owned(), Style::new().fg(style::red())),
        RunState::Fin => ("FIN".to_owned(), style::dim()),
    };
    Line::from(Span::styled(text, st))
}

fn interval_cell(r: &TowerRow) -> Line<'static> {
    let style = if r.drs {
        Style::new().fg(style::green())
    } else if r.interval == "Leader" {
        style::dim()
    } else {
        Style::new().fg(Color::White)
    };
    Line::from(Span::styled(r.interval.clone(), style))
}

/// `SOFT    12` for a new set, `MEDIUM   3*` for a used one.
fn tyre_cell(r: &TowerRow) -> Line<'static> {
    let Some(t) = &r.tyre else {
        return Line::from("");
    };
    let name = Style::new()
        .fg(style::compound(t.compound))
        .add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled(format!("{:<7}", t.compound.name()), name),
        Span::raw(format!(" {:>2}", t.age)),
        Span::styled(if t.new { "" } else { "*" }, style::dim()),
    ])
}

fn trend_cell(r: &TowerRow) -> Line<'static> {
    let Some(t) = r.trend else {
        return Line::from("");
    };
    if t.per_lap.abs() < TREND_DEADBAND {
        return Line::from(Span::styled(" =", style::dim()));
    }
    let (arrow, colour) = if t.per_lap > 0.0 {
        ("▲", style::green())
    } else {
        ("▼", style::red())
    };
    let mut spans = vec![Span::styled(
        format!("{arrow}{:.2}", t.per_lap.abs()),
        Style::new().fg(colour),
    )];
    if let Some(n) = t.laps_to_catch {
        spans.push(Span::styled(format!(" ~{n}L"), Style::new().fg(colour)));
    }
    Line::from(spans)
}

/// `▲0.25`: getting faster by a quarter of a second per lap.
fn pace_cell(r: &TowerRow) -> Line<'static> {
    let Some(p) = r.pace else {
        return Line::from("");
    };
    if p.abs() < TREND_DEADBAND {
        return Line::from(Span::styled(" =", style::dim()));
    }
    let (arrow, colour) = if p > 0.0 {
        ("▲", style::green())
    } else {
        ("▼", style::red())
    };
    Line::from(Span::styled(
        format!("{arrow}{:.2}", p.abs()),
        Style::new().fg(colour),
    ))
}

/// Flags shown to the driver as a badge: `B/W`, `B/O` or `DSQ`.
/// A flag badge, then penalties still to be served: `B/W +10s`,
/// `DT`, `SG`. Time penalties are added up.
fn penalty_cell(flag: Option<DriverFlag>, pens: &[Penalty]) -> Line<'static> {
    let mut spans = Vec::new();
    if let Some(flag) = flag {
        let (text, fg, bg) = match flag {
            DriverFlag::BlackWhite => ("B/W", Color::Black, Color::White),
            DriverFlag::BlackOrange => ("B/O", Color::Black, style::orange()),
            DriverFlag::Black => ("DSQ", Color::White, style::red()),
        };
        spans.push(Span::styled(
            format!(" {text} "),
            Style::new().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
        ));
    }
    let secs: u32 = pens
        .iter()
        .map(|p| match p {
            Penalty::Time(s) => *s,
            _ => 0,
        })
        .sum();
    let mut add = |text: String, colour: Color| {
        if !spans.is_empty() {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            text,
            Style::new().fg(colour).add_modifier(Modifier::BOLD),
        ));
    };
    if secs > 0 {
        add(format!("+{secs}s"), style::yellow());
    }
    for p in pens {
        match p {
            Penalty::DriveThrough => add("DT".into(), style::red()),
            Penalty::StopGo => add("SG".into(), style::red()),
            Penalty::Time(_) => {}
        }
    }
    Line::from(spans)
}

fn tag_cell(tag: Option<Tag>) -> Line<'static> {
    match tag {
        None => Line::from(""),
        // On the pit row's blue background.
        Some(Tag::Pit) => Line::from(Span::styled(
            "PIT",
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        )),
        Some(Tag::Ret) => Line::from(Span::styled("RET", Style::new().fg(style::red()))),
        Some(Tag::Fin) => Line::from(Span::styled(
            "FIN",
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        )),
    }
}

/// Whether a driver was given a penalty moments ago, in feed time. The
/// lower bound guards against a local clock running behind.
pub fn penalty_flashing(app: &App, num: &str) -> bool {
    app.state
        .penalised()
        .get(num)
        .is_some_and(|t| (-PENALTY_FLASH..PENALTY_FLASH).contains(&(app.now() - *t)))
}

/// Gold behind the driver who'd have the title if the race finished now,
/// flashing at first.
fn title_bg(app: &App, num: &str, blink: bool) -> Option<Color> {
    let (_, since) = app.state.title_secured().filter(|(n, _)| *n == num)?;
    let flashing = app.now() - since < TITLE_FLASH;
    (!(flashing && blink)).then(style::gold)
}

/// The championship fight above race control, when there is one.
fn draw_title_and_rc(f: &mut Frame, area: Rect, app: &App) {
    let lines = title_lines(app);
    if lines.is_empty() {
        return draw_rc(f, area, app);
    }
    let h = (lines.len() as u16 + 2).min(area.height / 2);
    let [top, bottom] = Layout::vertical([Constraint::Length(h), Constraint::Min(0)]).areas(area);
    f.render_widget(
        Paragraph::new(lines).block(panel("Championship fight")),
        top,
    );
    draw_rc(f, bottom, app);
}

/// Per driver who could secure the title in this race: what it takes,
/// each condition green while it's met.
fn title_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for b in &app.state.views.title {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        let ifs = if b.conditions.is_empty() { "" } else { ", if:" };
        let head = if !b.possible {
            Line::from(Span::styled(
                format!("{} - can no longer secure the title", b.tla),
                style::dim(),
            ))
        } else if b.secured {
            Line::from(vec![
                Span::styled(
                    b.tla.clone(),
                    Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" - Currently "),
                Span::styled(
                    "TITLE WINNER",
                    Style::new().fg(style::gold()).add_modifier(Modifier::BOLD),
                ),
                Span::raw(ifs),
            ])
        } else {
            Line::from(vec![
                Span::styled(
                    b.tla.clone(),
                    Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    " - Can win{}",
                    if ifs.is_empty() { "" } else { " if:" }
                )),
            ])
        };
        lines.push(head);
        for c in &b.conditions {
            let dir = match (c.own, c.pos) {
                (true, 1) => "",
                (true, _) => " or above",
                (false, _) => " or below",
            };
            let st = if c.met {
                Style::new().fg(style::green())
            } else {
                Style::new().fg(Color::Gray)
            };
            lines.push(Line::from(Span::styled(
                format!("- {} pos {}{dir}", c.tla, c.pos),
                st,
            )));
        }
    }
    lines
}

fn draw_rc(f: &mut Frame, area: Rect, app: &App) {
    let title = if app.rc_filter {
        "Race control (DRS/blue hidden)"
    } else {
        "Race control"
    };
    let block = panel(title);
    let inner = block.inner(area);
    let now = app.now();
    // "HH:MM:SS L12 ": wrapped text lines up under the message.
    const PREFIX: usize = 13;
    let text_w = (inner.width as usize).saturating_sub(PREFIX).max(10);
    let mut lines: Vec<Line> = Vec::new();
    for m in app
        .state
        .views
        .rcm
        .iter()
        .filter(|m| !(app.rc_filter && matches!(m.kind, RcmKind::Drs | RcmKind::Blue)))
    {
        if lines.len() >= inner.height as usize {
            break;
        }
        // Feed time, so replays highlight for 10 s of session time.
        // The lower bound guards against a local clock running behind.
        let new = m
            .utc
            .is_some_and(|t| (-RC_NEW..RC_NEW).contains(&(now - t)));
        let time = m
            .utc
            .map(|t| t.with_timezone(&Local).format("%H:%M:%S").to_string())
            .unwrap_or_else(|| " ".repeat(8));
        let pit_yellow = m.text.to_uppercase().contains("YELLOW IN PIT LANE");
        let lap = m
            .lap
            .map(|l| format!("L{l:<3}"))
            .unwrap_or_else(|| "    ".into());
        for (i, part) in wrap_words(&m.text, text_w).into_iter().enumerate() {
            let prefix = if i == 0 {
                format!("{time} {lap}")
            } else {
                " ".repeat(PREFIX)
            };
            // Each row is its own line so a highlight fills the whole row.
            let line = if pit_yellow {
                // Pit lane yellows have no flag of their own; always stand out.
                let st = Style::new()
                    .fg(Color::Black)
                    .bg(style::yellow())
                    .add_modifier(Modifier::BOLD);
                Line::from(vec![Span::raw(prefix), Span::raw(part)]).style(st)
            } else {
                let line = Line::from(vec![
                    Span::styled(prefix, style::dim()),
                    Span::styled(part, style::rcm(m.kind)),
                ]);
                if new {
                    line.style(Style::new().bg(style::rcm_new()))
                } else {
                    line
                }
            };
            lines.push(line);
        }
    }
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Splits `text` into lines of at most `width` characters at spaces,
/// breaking words longer than a line.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let mut word: Vec<char> = word.chars().collect();
        let cur_len = cur.chars().count();
        if cur_len > 0 && cur_len + 1 + word.len() <= width {
            cur.push(' ');
            cur.extend(&word);
            continue;
        }
        if cur_len > 0 {
            out.push(std::mem::take(&mut cur));
        }
        while word.len() > width {
            out.push(word.drain(..width).collect());
        }
        cur = word.into_iter().collect();
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

fn fmt_hms(d: TimeDelta) -> String {
    let s = d.num_seconds().max(0);
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn fmt_ms(d: TimeDelta) -> String {
    let s = d.num_seconds().max(0);
    format!("{}:{:02}", s / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_at_spaces() {
        assert_eq!(
            wrap_words("CAR 1 (VER) TIME 1:23.456 DELETED", 12),
            ["CAR 1 (VER)", "TIME", "1:23.456", "DELETED"]
        );
        assert_eq!(wrap_words("ABCDEFGHIJ KL", 4), ["ABCD", "EFGH", "IJ", "KL"]);
        assert_eq!(wrap_words("", 10), [""]);
    }

    #[test]
    fn pace_marks_each_mini_sector() {
        let sector = |segments: &[u32]| SectorSnap {
            value: String::new(),
            kind: TimeKind::Normal,
            segments: segments.to_vec(),
        };
        let lap = [sector(&[2051, 2049, 2048]), sector(&[2064, 0])];
        let line = mini_sector_cell(&lap);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "[●●●● ]");
        let fg: Vec<_> = line.spans[1..5].iter().map(|s| s.style.fg).collect();
        assert_eq!(
            fg,
            [
                Some(style::green()),
                Some(Color::White),
                Some(style::yellow()),
                Some(style::blue())
            ]
        );
    }

    #[test]
    fn weather_box() {
        let r = |value, delta| Reading { value, delta };
        let mut w = Weather {
            air: r(31.5, Some(0.2)),
            track: r(55.0, Some(-1.0)),
            humidity: r(38.5, Some(0.01)),
            wind: r(12.3, None),
            wind_dir: Some(337.0),
            rain: false,
        };
        let render = |w: &Weather| {
            let mut t =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(WEATHER_W, 7)).unwrap();
            t.draw(|f| draw_weather(f, f.area(), w)).unwrap();
            t.backend().buffer().clone()
        };
        let buf = render(&w);
        let text: Vec<String> = (1..6)
            .map(|y| {
                (1..WEATHER_W - 1)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            text,
            [
                "DRY",
                "Air    31.5°C ▲0.2",
                "Track  55.0°C ▼1.0",
                "Humid   38.5% =",
                "Wind  12.3m/s NNW",
            ]
        );
        assert_ne!(buf[(1, 1)].bg, style::wet());

        w.rain = true;
        let buf = render(&w);
        assert_eq!(buf[(1, 1)].symbol(), "W");
        assert_eq!(buf[(WEATHER_W - 2, 5)].bg, style::wet());
    }
}

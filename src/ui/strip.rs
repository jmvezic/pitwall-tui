//! The lap as a straight line: start/finish on the left, end of the lap on
//! the right. Cars sit on it by how far round the lap they are; marshal
//! sectors under a flag are coloured, timing sectors are marked.
//!
//! Progress comes from car positions projected onto the circuit outline
//! when the feed has them, else from the timing mini-sectors each car has
//! passed this lap, which need no F1 TV token.

use std::collections::{HashMap, HashSet};

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Block;
use serde_json::Value;

use super::style;
use crate::circuit::Circuit;
use crate::state::State;
use crate::state::derive::{SectorFlag, Tag, TrackState};
use crate::state::merge::indexed;
use crate::state::parse::as_u32;

/// Position samples older than this (feed time) aren't used.
const POS_FRESH: chrono::TimeDelta = chrono::TimeDelta::seconds(10);

/// Circuit geometry reduced to distance along the lap.
pub struct TrackLine {
    pts: Vec<(f64, f64)>,
    /// Distance from the start of the outline to each point.
    cum: Vec<f64>,
    /// Lap length, including the segment closing the loop.
    total: f64,
    /// Lap fraction at the end of each timing mini-sector.
    mini: Vec<f64>,
    /// Marshal sector number and the lap fraction where it starts.
    marshal: Vec<(u32, f64)>,
}

impl TrackLine {
    /// The outline starts at the timing line and runs in racing direction.
    pub fn new(c: &Circuit) -> Self {
        let pts: Vec<(f64, f64)> = c.x.iter().copied().zip(c.y.iter().copied()).collect();
        let mut cum = Vec::with_capacity(pts.len());
        let mut d = 0.0;
        for (i, p) in pts.iter().enumerate() {
            if i > 0 {
                d += dist(pts[i - 1], *p);
            }
            cum.push(d);
        }
        let total = d + pts
            .last()
            .zip(pts.first())
            .map_or(0.0, |(a, b)| dist(*a, *b));
        let mut line = Self {
            pts,
            cum,
            total: total.max(1.0),
            mini: Vec::new(),
            marshal: Vec::new(),
        };
        line.mini = c
            .mini_sectors_indexes
            .iter()
            .filter(|&&i| i < line.pts.len())
            .map(|&i| line.cum[i] / line.total)
            .collect();
        line.marshal = c
            .marshal_sectors
            .iter()
            .map(|m| {
                (
                    m.number,
                    line.frac_at(m.track_position.x, m.track_position.y),
                )
            })
            .collect();
        line.marshal.sort_by_key(|m| m.0);
        line
    }

    pub fn has_marshal_sectors(&self) -> bool {
        !self.marshal.is_empty()
    }

    /// Marshal sector numbers in order.
    pub fn marshal_numbers(&self) -> Vec<u32> {
        self.marshal.iter().map(|m| m.0).collect()
    }

    /// Lap fraction of the outline point nearest to `(x, y)`, refined by
    /// projecting onto the neighbouring outline segments.
    pub fn frac_at(&self, x: f64, y: f64) -> f64 {
        let n = self.pts.len();
        if n < 2 {
            return 0.0;
        }
        let p = (x, y);
        let i = (0..n)
            .min_by(|&a, &b| dist(self.pts[a], p).total_cmp(&dist(self.pts[b], p)))
            .unwrap_or(0);
        let mut best = (dist(self.pts[i], p), self.cum[i]);
        for (a, b) in [((i + n - 1) % n, i), (i, (i + 1) % n)] {
            let (pa, pb) = (self.pts[a], self.pts[b]);
            let len = dist(pa, pb);
            if len < 1e-9 {
                continue;
            }
            let t = (((p.0 - pa.0) * (pb.0 - pa.0) + (p.1 - pa.1) * (pb.1 - pa.1)) / (len * len))
                .clamp(0.0, 1.0);
            let q = (pa.0 + t * (pb.0 - pa.0), pa.1 + t * (pb.1 - pa.1));
            let d = dist(q, p);
            if d < best.0 {
                // The closing segment starts at the last point.
                let start = if b == 0 { self.cum[n - 1] } else { self.cum[a] };
                best = (d, start + t * len);
            }
        }
        (best.1 / self.total).rem_euclid(1.0)
    }

    /// The marshal sector at a lap fraction.
    fn marshal_at(&self, frac: f64) -> Option<u32> {
        let n = self.marshal.len();
        (0..n).find_map(|i| {
            let (num, start) = self.marshal[i];
            let end = self.marshal[(i + 1) % n].1;
            let inside = if start <= end {
                frac >= start && frac < end
            } else {
                frac >= start || frac < end
            };
            inside.then_some(num)
        })
    }

    /// Middle of a marshal sector, for its label.
    fn marshal_mid(&self, num: u32) -> Option<f64> {
        let n = self.marshal.len();
        let i = self.marshal.iter().position(|m| m.0 == num)?;
        let start = self.marshal[i].1;
        let mut end = self.marshal[(i + 1) % n].1;
        if end <= start {
            end += 1.0;
        }
        Some(((start + end) / 2.0).rem_euclid(1.0))
    }
}

fn dist(a: (f64, f64), b: (f64, f64)) -> f64 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

/// Lap fraction after `passed` of `count` mini-sectors.
fn segments_frac(mini: &[f64], passed: usize, count: usize) -> f64 {
    match passed {
        0 => 0.0,
        _ if mini.len() == count => mini[passed - 1],
        _ => passed as f64 / count.max(1) as f64,
    }
}

/// Mini-sectors passed this lap and the total, from a TimingData line.
/// Passed segments have a non-zero status; they reset when a lap starts.
pub fn segment_progress(line: &Value) -> Option<(usize, usize)> {
    let mut count = 0;
    let mut passed = 0;
    for sector in indexed(&line["Sectors"]) {
        for seg in indexed(&sector["Segments"]) {
            count += 1;
            if as_u32(&seg["Status"]).is_some_and(|s| s != 0) {
                passed = count;
            }
        }
    }
    (count > 0).then_some((passed, count))
}

/// Mini-sector counts of the three timing sectors.
fn sector_sizes(timing: &Value) -> Option<Vec<usize>> {
    let line = timing["Lines"].as_object()?.values().next()?;
    let sizes: Vec<usize> = indexed(&line["Sectors"])
        .iter()
        .map(|s| indexed(&s["Segments"]).len())
        .collect();
    (sizes.iter().sum::<usize>() > 0).then_some(sizes)
}

pub struct StripCtx<'a> {
    pub line: Option<&'a TrackLine>,
    pub state: &'a State,
    pub selected: Option<&'a str>,
    pub blink: bool,
}

/// Whether car progress comes from positions (smooth) or mini-sectors.
pub fn uses_positions(ctx: &StripCtx) -> bool {
    ctx.line.is_some_and(|l| l.pts.len() >= 2) && ctx.state.cars.has_positions()
}

pub fn draw(f: &mut Frame, area: Rect, block: Block, ctx: &StripCtx) {
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 10 || inner.height < 3 {
        return;
    }
    let buf = f.buffer_mut();
    let w = inner.width;
    let lanes = (inner.height - 3).clamp(1, 3);
    let line_y = inner.y + lanes;
    let label_y = line_y + 1;
    let pit_y = line_y + 2;
    let col = |frac: f64| inner.x + (frac.clamp(0.0, 1.0) * f64::from(w - 1)).round() as u16;
    let frac_of_col = |c: u16| f64::from(c - inner.x) / f64::from(w - 1);

    let v = &ctx.state.views;
    let timing = ctx.state.topics.get("TimingData").unwrap_or(&Value::Null);
    let mini: &[f64] = ctx.line.map_or(&[], |l| &l.mini);

    // The track follows the track status the way the status box does:
    // SC and VSC flash orange, their endings are steady orange, red is red.
    // Flagged marshal sectors are yellow on top; double yellows flash.
    let grey = Style::new().fg(style::rgb(90, 90, 90));
    let base = match v.track {
        TrackState::SafetyCar | TrackState::Vsc if ctx.blink => grey,
        TrackState::SafetyCar
        | TrackState::Vsc
        | TrackState::SafetyCarEnding
        | TrackState::VscEnding => Style::new()
            .fg(style::orange())
            .add_modifier(Modifier::BOLD),
        TrackState::Red => Style::new().fg(style::red()).add_modifier(Modifier::BOLD),
        _ => grey,
    };
    let flagged = |frac: f64| -> Option<SectorFlag> {
        let line = ctx.line?;
        v.sector_flags.get(&line.marshal_at(frac)?).copied()
    };
    for c in inner.x..inner.x + w {
        let style = match flagged(frac_of_col(c)).filter(|_| v.track != TrackState::Red) {
            Some(fl) => style::sector_flag(fl, ctx.blink)
                .map_or(base, |c| Style::new().fg(c).add_modifier(Modifier::BOLD)),
            None => base,
        };
        buf.set_string(c, line_y, "━", style);
    }

    // Timing sector boundaries and labels.
    let label = Style::new().fg(style::rgb(150, 150, 150));
    buf.set_string(inner.x, label_y, "S1", label);
    if let Some(sizes) = sector_sizes(timing) {
        let count: usize = sizes.iter().sum();
        let mut passed = 0;
        for (i, n) in sizes.iter().enumerate().take(sizes.len() - 1) {
            passed += n;
            let c = col(segments_frac(mini, passed, count));
            buf.set_string(c, line_y, "┃", Style::new().fg(Color::White));
            buf.set_string(c, label_y, format!("S{}", i + 2), label);
        }
    }
    let fin = "FINISH";
    buf.set_string(inner.x + w - fin.len() as u16, label_y, fin, label);

    // Numbers under flagged marshal sectors.
    if let Some(line) = ctx.line {
        for num in v.sector_flags.keys() {
            if let Some(mid) = line.marshal_mid(*num) {
                let text = num.to_string();
                let c = col(mid).saturating_sub(text.len() as u16 / 2).max(inner.x);
                let c = c.min(inner.x + w - text.len() as u16);
                let st = Style::new()
                    .fg(style::yellow())
                    .add_modifier(Modifier::BOLD);
                // Keep a gap around it so it can't merge with S2/FINISH.
                let lo = c.saturating_sub(1).max(inner.x);
                let hi = (c + text.len() as u16 + 1).min(inner.x + w);
                if (lo..hi).all(|x| is_blank(buf, x, label_y)) {
                    buf.set_string(c, label_y, text, st);
                }
            }
        }
    }

    // Cars.
    let stopped: HashSet<&str> = v.stopped.iter().map(|s| s.num.as_str()).collect();
    let positions = uses_positions(ctx);
    let now = ctx.state.feed_utc;
    let mut on_track: Vec<(&crate::state::derive::TowerRow, f64)> = Vec::new();
    let mut in_pit = Vec::new();
    for row in &v.tower {
        match row.tag {
            Some(Tag::Ret) => continue,
            Some(Tag::Pit) => {
                in_pit.push(row);
                continue;
            }
            Some(Tag::Fin) | None => {}
        }
        let from_pos = ctx.line.filter(|_| positions).and_then(|l| {
            let p = ctx.state.cars.positions().find(|(n, _)| *n == &row.num)?.1;
            let fresh = now.is_none_or(|t| t - p.ts <= POS_FRESH);
            fresh.then(|| l.frac_at(p.x, p.y))
        });
        let frac = from_pos.or_else(|| {
            let (passed, count) = segment_progress(&timing["Lines"][&row.num])?;
            Some(segments_frac(mini, passed, count))
        });
        if let Some(frac) = frac {
            on_track.push((row, frac));
        }
    }

    // Dots: leader drawn last so it stays on top.
    for (row, frac) in on_track.iter().rev() {
        let mut st = Style::new()
            .fg(style::team(row.colour))
            .add_modifier(Modifier::BOLD);
        if stopped.contains(row.num.as_str()) && ctx.blink {
            st = st.fg(style::red());
        }
        buf.set_string(col(*frac), line_y, "●", st);
    }

    // Labels: selected, stopped and the leader first, then by position.
    let mut order: Vec<&(&crate::state::derive::TowerRow, f64)> = on_track.iter().collect();
    let rank = |r: &crate::state::derive::TowerRow| {
        if Some(r.num.as_str()) == ctx.selected {
            0
        } else if stopped.contains(r.num.as_str()) {
            1
        } else {
            2
        }
    };
    order.sort_by_key(|(r, _)| (rank(r), r.pos));
    let mut used: HashMap<u16, Vec<bool>> = HashMap::new();
    let mut stems = Vec::new();
    for (row, frac) in order {
        let text = &row.tla;
        let len = text.chars().count() as u16;
        let x = col(*frac);
        let start = x.saturating_sub(len / 2).clamp(inner.x, inner.x + w - len);
        for lane in 0..lanes {
            let y = line_y - 1 - lane;
            let cells = used.entry(y).or_insert_with(|| vec![false; w as usize]);
            let lo = (start - inner.x).saturating_sub(1) as usize;
            let hi = ((start - inner.x + len + 1) as usize).min(w as usize);
            if cells[lo..hi].iter().any(|&u| u) {
                continue;
            }
            cells[lo..hi].iter_mut().for_each(|u| *u = true);
            let mut st = Style::new()
                .fg(style::team(row.colour))
                .add_modifier(Modifier::BOLD);
            if stopped.contains(row.num.as_str()) {
                st = Style::new()
                    .fg(Color::White)
                    .bg(style::red())
                    .add_modifier(Modifier::BOLD);
            }
            if Some(row.num.as_str()) == ctx.selected {
                st = st.add_modifier(Modifier::REVERSED);
            }
            buf.set_string(start, y, text, st);
            if lane > 0 {
                stems.push((x, y + 1, line_y));
            }
            break;
        }
    }
    // Thin stems from higher labels down to their car, where there's room.
    for (x, from, to) in stems {
        for y in from..to {
            if is_blank(buf, x, y) {
                buf.set_string(x, y, "╎", style::dim());
            }
        }
    }

    // Cars in the pit lane.
    if !in_pit.is_empty() && pit_y < inner.y + inner.height {
        let mut x = inner.x;
        buf.set_string(x, pit_y, "PIT LANE", label);
        x += 9;
        for row in in_pit {
            if x + 4 > inner.x + w {
                break;
            }
            let st = Style::new()
                .fg(style::team(row.colour))
                .add_modifier(Modifier::BOLD);
            buf.set_string(x, pit_y, &row.tla, st);
            x += row.tla.chars().count() as u16 + 1;
        }
    }
}

fn is_blank(buf: &Buffer, x: u16, y: u16) -> bool {
    buf.cell((x, y)).is_some_and(|c| c.symbol() == " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::circuit::{Marker, Point};
    use serde_json::json;

    /// A 100×100 square, start at the origin, anticlockwise.
    fn square() -> Circuit {
        let (mut x, mut y) = (Vec::new(), Vec::new());
        for i in 0..40 {
            let (px, py) = match i / 10 {
                0 => (i * 10, 0),
                1 => (100, (i - 10) * 10),
                2 => (100 - (i - 20) * 10, 100),
                _ => (0, 100 - (i - 30) * 10),
            };
            x.push(f64::from(px));
            y.push(f64::from(py));
        }
        let m = |number, x, y| Marker {
            number,
            track_position: Point { x, y },
        };
        Circuit {
            x,
            y,
            rotation: 0.0,
            marshal_sectors: vec![
                m(1, 0.0, 0.0),
                m(2, 100.0, 0.0),
                m(3, 100.0, 100.0),
                m(4, 0.0, 100.0),
            ],
            corners: vec![],
            mini_sectors_indexes: vec![9, 19, 29, 39],
        }
    }

    #[test]
    fn positions_map_to_lap_fraction() {
        let l = TrackLine::new(&square());
        assert!((l.frac_at(0.0, 0.0) - 0.0).abs() < 1e-9);
        assert!((l.frac_at(100.0, 0.0) - 0.25).abs() < 1e-9);
        assert!((l.frac_at(55.0, 103.0) - 0.6125).abs() < 1e-9); // off-line, projected
        assert!((l.frac_at(0.0, 5.0) - 0.9875).abs() < 1e-9); // on the closing segment
    }

    #[test]
    fn marshal_sectors_and_mini_sectors() {
        let l = TrackLine::new(&square());
        assert_eq!(l.marshal_at(0.1), Some(1));
        assert_eq!(l.marshal_at(0.8), Some(4));
        assert_eq!(l.marshal_mid(4), Some(0.875));
        assert!((segments_frac(&l.mini, 2, 4) - 0.475).abs() < 1e-9);
        // Mini-sector count doesn't match the geometry: spread evenly.
        assert!((segments_frac(&l.mini, 3, 6) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn segment_progress_counts_passed() {
        let line = json!({"Sectors": [
            {"Segments": [{"Status": 2049}, {"Status": 2048}, {"Status": 0}]},
            {"Segments": [{"Status": 0}, {"Status": 0}]},
        ]});
        assert_eq!(segment_progress(&line), Some((2, 5)));
        assert_eq!(segment_progress(&json!({})), None);
    }
}

//! Colours, with a 256-colour fallback for terminals without true colour.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};

use crate::state::derive::{RcmKind, SectorFlag, TimeKind, TrackState};
use crate::state::parse::Compound;

fn truecolor() -> bool {
    static TC: OnceLock<bool> = OnceLock::new();
    *TC.get_or_init(|| {
        std::env::var("COLORTERM").is_ok_and(|v| {
            let v = v.to_ascii_lowercase();
            v.contains("truecolor") || v.contains("24bit")
        })
    })
}

pub fn rgb(r: u8, g: u8, b: u8) -> Color {
    if truecolor() {
        Color::Rgb(r, g, b)
    } else {
        Color::Indexed(to_256(r, g, b))
    }
}

/// Nearest xterm-256 colour: the 6×6×6 cube or the grey ramp.
pub fn to_256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let nearest = |c: u8| {
        (0..6)
            .min_by_key(|&i| (i32::from(LEVELS[i]) - i32::from(c)).abs())
            .unwrap_or(0)
    };
    let (ri, gi, bi) = (nearest(r), nearest(g), nearest(b));
    let cube = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);
    let grey_level = ((u32::from(r) + u32::from(g) + u32::from(b)) / 3) as u8;
    let gi_ramp = (i32::from(grey_level) - 8).clamp(0, 230) / 10;
    let grey = (8 + gi_ramp * 10) as u8;
    let dist = |(a, b2, c): (u8, u8, u8)| {
        let d = |x: u8, y: u8| (i32::from(x) - i32::from(y)).pow(2);
        d(a, r) + d(b2, g) + d(c, b)
    };
    if dist((grey, grey, grey)) < dist(cube) {
        232 + gi_ramp as u8
    } else {
        16 + 36 * ri as u8 + 6 * gi as u8 + bi as u8
    }
}

pub fn team(c: Option<(u8, u8, u8)>) -> Color {
    c.map_or(Color::Gray, |(r, g, b)| rgb(r, g, b))
}

pub fn dim() -> Style {
    Style::new().fg(rgb(110, 110, 110))
}

pub fn green() -> Color {
    rgb(60, 210, 90)
}

pub fn red() -> Color {
    rgb(235, 50, 50)
}

pub fn yellow() -> Color {
    rgb(250, 215, 40)
}

pub fn orange() -> Color {
    rgb(255, 140, 0)
}

pub fn purple() -> Color {
    rgb(190, 90, 255)
}

pub fn blue() -> Color {
    rgb(60, 140, 255)
}

pub fn compound(c: Compound) -> Color {
    match c {
        Compound::Soft => red(),
        Compound::Medium => yellow(),
        Compound::Hard => rgb(240, 240, 240),
        Compound::Intermediate | Compound::Wet => blue(),
        Compound::Unknown => Color::Gray,
    }
}

pub fn laptime(k: TimeKind) -> Style {
    match k {
        TimeKind::Overall => Style::new().fg(purple()),
        TimeKind::Personal => Style::new().fg(green()),
        TimeKind::Normal => Style::new().fg(Color::White),
    }
}

/// Banner text and background.
/// Banner text, foreground and background. SC and VSC flash: on the off
/// phase (`blink`) the colours invert onto a dark background.
pub fn banner(t: &TrackState, blink: bool) -> (&'static str, Color, Color) {
    let (text, fg, bg) = match t {
        TrackState::Clear => ("TRACK CLEAR", Color::Black, green()),
        TrackState::Yellow => ("YELLOW", Color::Black, yellow()),
        TrackState::SafetyCar => ("SAFETY CAR", Color::Black, orange()),
        TrackState::SafetyCarEnding => ("SAFETY CAR ENDING", Color::Black, orange()),
        TrackState::Red => ("RED FLAG", Color::White, red()),
        TrackState::Vsc => ("VIRTUAL SAFETY CAR", Color::Black, orange()),
        TrackState::VscEnding => ("VIRTUAL SAFETY CAR ENDING", Color::Black, orange()),
        TrackState::Unknown => ("NO STATUS", Color::White, rgb(70, 70, 70)),
    };
    if blink && t.flashes() {
        (text, bg, flash_off())
    } else {
        (text, fg, bg)
    }
}

/// Background on the off phase of anything flashing.
pub fn flash_off() -> Color {
    rgb(45, 45, 45)
}

/// Background for a race control message that just arrived.
pub fn rcm_new() -> Color {
    rgb(35, 55, 95)
}

/// Background for a car in the pit lane.
pub fn pit_row() -> Color {
    rgb(25, 60, 125)
}

/// Background of the weather box while it rains.
pub fn wet() -> Color {
    rgb(30, 70, 150)
}

/// Qualifying: a new leader's row, for a moment.
pub fn new_leader() -> Color {
    rgb(255, 110, 200)
}

/// The driver who'd have the title if the race finished now.
pub fn gold() -> Color {
    rgb(232, 185, 35)
}

/// Background of every other tower row.
pub fn row_stripe() -> Color {
    rgb(28, 28, 28)
}

/// A retired car's tower row: everything grey, nothing bold.
pub fn retired() -> Style {
    Style::new().fg(rgb(85, 85, 85))
}

/// Background for a stopped car's tower row.
pub fn stopped_row() -> Color {
    rgb(130, 25, 25)
}

/// Sector colour; a double yellow is yellow that flashes, so on the off
/// phase it returns `None`.
pub fn sector_flag(f: SectorFlag, blink: bool) -> Option<Color> {
    match f {
        SectorFlag::DoubleYellow if blink => None,
        _ => Some(yellow()),
    }
}

pub fn rcm(k: RcmKind) -> Style {
    match k {
        RcmKind::Green => Style::new().fg(green()),
        RcmKind::Yellow => Style::new().fg(yellow()),
        RcmKind::Red => Style::new().fg(red()).add_modifier(Modifier::BOLD),
        RcmKind::Chequered => Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        RcmKind::SafetyCar => Style::new().fg(orange()),
        RcmKind::Penalty => Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        RcmKind::Drs | RcmKind::Blue => dim(),
        RcmKind::Other => Style::new().fg(Color::Gray),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_256() {
        assert_eq!(to_256(255, 0, 0), 196);
        assert_eq!(to_256(0, 0, 0), 16);
        assert_eq!(to_256(128, 128, 128), 244);
        assert_eq!(to_256(255, 128, 0), 208);
    }
}

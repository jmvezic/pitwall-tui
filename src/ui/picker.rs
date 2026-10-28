//! Browsing the archive: a season's meetings, then a meeting's sessions.

use chrono::NaiveDateTime;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use super::draw::panel;
use super::style;
use crate::feed::archive::{Index, Meeting, Session};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Meetings,
    Sessions,
}

struct Picker<'a> {
    index: &'a Index,
    level: Level,
    meeting: usize,
    session: usize,
}

/// What a key press did.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Continue,
    Quit,
    /// Meeting and session indexes.
    Chosen(usize, usize),
}

impl<'a> Picker<'a> {
    /// Starts at the most recent meeting, or at `meeting`'s sessions.
    fn new(index: &'a Index, meeting: Option<usize>) -> Self {
        let mut p = Self {
            index,
            level: Level::Meetings,
            meeting: index.meetings.len().saturating_sub(1),
            session: 0,
        };
        if let Some(m) = meeting {
            p.meeting = m;
            p.open_meeting();
        }
        p
    }

    fn current(&self) -> Option<&'a Meeting> {
        self.index.meetings.get(self.meeting)
    }

    /// Shows the meeting's sessions with the race (or the last archived
    /// session) selected.
    fn open_meeting(&mut self) {
        let Some(m) = self.current() else {
            return;
        };
        self.level = Level::Sessions;
        self.session = m
            .sessions
            .iter()
            .position(|s| s.name == "Race" && s.path.is_some())
            .or_else(|| m.sessions.iter().rposition(|s| s.path.is_some()))
            .unwrap_or(0);
    }

    fn len(&self) -> usize {
        match self.level {
            Level::Meetings => self.index.meetings.len(),
            Level::Sessions => self.current().map_or(0, |m| m.sessions.len()),
        }
    }

    fn cursor(&mut self) -> &mut usize {
        match self.level {
            Level::Meetings => &mut self.meeting,
            Level::Sessions => &mut self.session,
        }
    }

    fn key(&mut self, code: KeyCode, mods: KeyModifiers) -> Outcome {
        let last = self.len().saturating_sub(1);
        match code {
            KeyCode::Char('q') => return Outcome::Quit,
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => return Outcome::Quit,
            KeyCode::Up | KeyCode::Char('k') => {
                let c = self.cursor();
                *c = c.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let c = self.cursor();
                *c = (*c + 1).min(last);
            }
            KeyCode::Home | KeyCode::Char('g') => *self.cursor() = 0,
            KeyCode::End | KeyCode::Char('G') => *self.cursor() = last,
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => match self.level {
                Level::Meetings => self.open_meeting(),
                Level::Sessions => {
                    let archived = self
                        .current()
                        .and_then(|m| m.sessions.get(self.session))
                        .is_some_and(|s| s.path.is_some());
                    if archived {
                        return Outcome::Chosen(self.meeting, self.session);
                    }
                }
            },
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
                match self.level {
                    Level::Sessions => self.level = Level::Meetings,
                    Level::Meetings if code == KeyCode::Esc => return Outcome::Quit,
                    Level::Meetings => {}
                }
            }
            _ => {}
        }
        Outcome::Continue
    }

    fn draw(&self, f: &mut ratatui::Frame) {
        let [head, body, foot] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(f.area());

        let mut crumbs = vec![
            Span::styled(" pitwall ", Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(format!("· replay · {}", self.index.year), style::dim()),
        ];
        if self.level == Level::Sessions
            && let Some(m) = self.current()
        {
            crumbs.push(Span::styled(format!(" · {}", m.name), style::dim()));
        }
        f.render_widget(Paragraph::new(Line::from(crumbs)), head);

        let (title, items, selected) = match self.level {
            Level::Meetings => (
                format!("{} season", self.index.year),
                self.index.meetings.iter().map(meeting_item).collect(),
                self.meeting,
            ),
            Level::Sessions => match self.current() {
                Some(m) => (
                    m.name.clone(),
                    m.sessions.iter().map(session_item).collect(),
                    self.session,
                ),
                None => (String::new(), Vec::new(), 0),
            },
        };
        let list = List::new(items)
            .block(panel(&title))
            .highlight_style(Style::new().bg(style::rgb(45, 45, 70)))
            .highlight_symbol("▶ ");
        let mut state = ListState::default().with_selected(Some(selected));
        f.render_stateful_widget(list, body, &mut state);

        let keys = match self.level {
            Level::Meetings => " ↑↓ select  enter open  esc/q quit",
            Level::Sessions => " ↑↓ select  enter replay  esc back  q quit",
        };
        f.render_widget(Paragraph::new(Span::styled(keys, style::dim())), foot);
    }
}

/// `11 Oct  Singapore Grand Prix          Marina Bay`, dated by its last
/// session.
fn meeting_item(m: &Meeting) -> ListItem<'static> {
    let date = m
        .sessions
        .last()
        .and_then(|s| parse_local(&s.start_date))
        .map(|d| d.format("%e %b").to_string())
        .unwrap_or_default();
    let archived = m.sessions.iter().any(|s| s.path.is_some());
    let name = if archived {
        Style::new().fg(Color::White)
    } else {
        style::dim()
    };
    ListItem::new(Line::from(vec![
        Span::styled(format!("{date:>6}  "), style::dim()),
        Span::styled(format!("{:<28} ", m.name), name),
        Span::styled(m.location.clone(), style::dim()),
    ]))
}

/// `Sun 11 Oct 20:00  Race`, in the circuit's local time.
fn session_item(s: &Session) -> ListItem<'static> {
    let when = parse_local(&s.start_date)
        .map(|d| d.format("%a %e %b %H:%M").to_string())
        .unwrap_or_default();
    let mut spans = vec![Span::styled(format!("{when:>16}  "), style::dim())];
    if s.path.is_some() {
        spans.push(Span::styled(
            s.name.clone(),
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::styled(s.name.clone(), style::dim()));
        spans.push(Span::styled("  not archived", style::dim()));
    }
    ListItem::new(Line::from(spans))
}

fn parse_local(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s.get(..19)?, "%Y-%m-%dT%H:%M:%S").ok()
}

/// Lets the user pick a session; `meeting` opens straight at its sessions.
/// Returns meeting and session indexes, or `None` if they quit.
pub fn pick(
    terminal: &mut DefaultTerminal,
    index: &Index,
    meeting: Option<usize>,
) -> anyhow::Result<Option<(usize, usize)>> {
    let mut p = Picker::new(index, meeting);
    loop {
        terminal.draw(|f| p.draw(f))?;
        let Event::Key(k) = event::read()? else {
            continue;
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        match p.key(k.code, k.modifiers) {
            Outcome::Continue => {}
            Outcome::Quit => return Ok(None),
            Outcome::Chosen(m, s) => return Ok(Some((m, s))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Index {
        serde_json::from_value(serde_json::json!({
            "Year": 2026,
            "Meetings": [
                {"Name": "Monaco Grand Prix", "Location": "Monaco", "Sessions": [
                    {"Name": "Qualifying", "StartDate": "2026-06-06T16:00:00", "Path": "q/"},
                    {"Name": "Race", "StartDate": "2026-06-07T15:00:00", "Path": "r/"},
                ]},
                {"Name": "Singapore Grand Prix", "Location": "Marina Bay", "Sessions": [
                    {"Name": "Sprint", "StartDate": "2026-10-10T17:00:00", "Path": "s/"},
                    {"Name": "Race", "StartDate": "2026-10-11T20:00:00"},
                ]},
            ]
        }))
        .unwrap()
    }

    #[test]
    fn browse_meetings_then_sessions() {
        let idx = index();
        let none = KeyModifiers::NONE;
        let mut p = Picker::new(&idx, None);
        // Starts at the most recent meeting.
        assert_eq!((p.level, p.meeting), (Level::Meetings, 1));
        p.key(KeyCode::Up, none);
        assert_eq!(p.key(KeyCode::Enter, none), Outcome::Continue);
        // Monaco's race is preselected.
        assert_eq!((p.level, p.session), (Level::Sessions, 1));
        p.key(KeyCode::Esc, none);
        assert_eq!(p.level, Level::Meetings);
        p.key(KeyCode::Down, none);
        p.key(KeyCode::Enter, none);
        // Singapore's race isn't archived: the sprint is preselected, and
        // the race can't be chosen.
        assert_eq!(p.session, 0);
        p.key(KeyCode::Down, none);
        assert_eq!(p.key(KeyCode::Enter, none), Outcome::Continue);
        p.key(KeyCode::Up, none);
        assert_eq!(p.key(KeyCode::Enter, none), Outcome::Chosen(1, 0));
        assert_eq!(p.key(KeyCode::Char('q'), none), Outcome::Quit);
    }

    #[test]
    fn opens_at_a_meeting() {
        let idx = index();
        let p = Picker::new(&idx, Some(0));
        assert_eq!((p.level, p.meeting, p.session), (Level::Sessions, 0, 1));
    }
}

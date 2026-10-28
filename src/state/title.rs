//! Whether a driver can secure the drivers' title in this race, and what it
//! takes: where they must finish, and where each rival who could still
//! catch them must finish.
//!
//! Secured means no rival can get ahead even by scoring the most points in
//! every round left. A rival who can only draw level loses the tie-break
//! when they'd have fewer grand prix wins, even winning every one left.
//! Equal wins go to further countback (second places, …), which we can't
//! see, so that isn't counted as secured.

/// Points float in the feed (`325.0`); half points have happened.
const EPS: f64 = 1e-6;

/// Points by finishing position.
#[derive(Debug, Clone, PartialEq)]
pub struct Rules {
    pub gp: Vec<f64>,
    pub sprint: Vec<f64>,
    /// A point for the fastest lap, for a top-10 finisher (2019–2024).
    pub fastest_lap: bool,
}

impl Rules {
    pub fn for_year(year: i32) -> Self {
        let sprint = if year == 2021 {
            vec![3.0, 2.0, 1.0]
        } else {
            vec![8.0, 7.0, 6.0, 5.0, 4.0, 3.0, 2.0, 1.0]
        };
        Self {
            gp: vec![25.0, 18.0, 15.0, 12.0, 10.0, 8.0, 6.0, 4.0, 2.0, 1.0],
            sprint,
            fastest_lap: (2019..=2024).contains(&year),
        }
    }

    fn gp_max(&self) -> f64 {
        self.gp[0] + if self.fastest_lap { 1.0 } else { 0.0 }
    }

    fn sprint_max(&self) -> f64 {
        self.sprint[0]
    }
}

/// A driver going into the session.
#[derive(Debug, Clone)]
pub struct Entry {
    pub num: String,
    pub tla: String,
    /// Points before this session.
    pub points: f64,
    /// Grand prix wins before this session.
    pub wins: u32,
    /// Where they're running; `None` once retired.
    pub pos: Option<u32>,
}

/// The session and what's left after it.
#[derive(Debug, Clone)]
pub struct Fight {
    pub entries: Vec<Entry>,
    /// Points by position in this session.
    pub table: Vec<f64>,
    /// A grand prix: a win here counts for the tie-break, and in fastest
    /// lap years a rival in the top 10 may add a point.
    pub gp: bool,
    pub fastest_lap: bool,
    /// The most points anyone can score after this session.
    pub left: f64,
    /// Grand prix still to run after this session.
    pub later_gps: u32,
}

impl Fight {
    /// `later`: the rounds after this one, whether each has a sprint. A
    /// sprint is followed by its own weekend's grand prix.
    pub fn new(entries: Vec<Entry>, rules: &Rules, sprint: bool, later: &[bool]) -> Self {
        let later_gps = later.len() as u32 + u32::from(sprint);
        let left = f64::from(later_gps) * rules.gp_max()
            + later.iter().filter(|&&s| s).count() as f64 * rules.sprint_max();
        Self {
            entries,
            table: if sprint {
                rules.sprint.clone()
            } else {
                rules.gp.clone()
            },
            gp: !sprint,
            fastest_lap: rules.fastest_lap && !sprint,
            left,
            later_gps,
        }
    }

    fn scored(&self, pos: Option<u32>) -> f64 {
        pos.and_then(|p| self.table.get(p as usize - 1))
            .copied()
            .unwrap_or(0.0)
    }

    fn won(&self, pos: Option<u32>) -> u32 {
        u32::from(self.gp && pos == Some(1))
    }

    /// Whether `l` finishing at `pl` stays ahead of `r` finishing at `pr`
    /// whatever happens after. `None` scores nothing.
    fn beats(&self, l: usize, pl: Option<u32>, r: usize, pr: Option<u32>) -> bool {
        let (a, b) = (&self.entries[l], &self.entries[r]);
        // Assume the rival takes the fastest lap point if they can.
        let bonus = if self.fastest_lap && pr.is_some_and(|p| p <= 10) {
            1.0
        } else {
            0.0
        };
        let lead = a.points + self.scored(pl) - (b.points + self.scored(pr) + bonus);
        if lead > self.left + EPS {
            return true;
        }
        // Level at best: the rival wins every grand prix left.
        (lead - self.left).abs() < EPS
            && a.wins + self.won(pl) > b.wins + self.won(pr) + self.later_gps
    }

    /// Whether `l` finishing at `pl` secures the title when every rival
    /// scores nothing.
    fn possible_from(&self, l: usize, pl: Option<u32>) -> bool {
        (0..self.entries.len()).all(|r| r == l || self.beats(l, pl, r, None))
    }

    /// Where `l` must finish: `Some(None)` when anywhere will do,
    /// `Some(Some(p))` for P`p` or better, `None` when it can't be done.
    fn needs(&self, l: usize) -> Option<Option<u32>> {
        if self.possible_from(l, None) {
            return Some(None);
        }
        // A retired driver can't move up.
        self.entries[l].pos?;
        (1..=self.entries.len() as u32)
            .rev()
            .find(|&p| self.possible_from(l, Some(p)))
            .map(Some)
    }

    /// The highest place `r` can take with `l` at `pl` and `l` still
    /// securing it; `None` when that's anywhere `l` isn't.
    fn rival_limit(&self, l: usize, pl: Option<u32>, r: usize) -> Option<u32> {
        let n = self.entries.len() as u32;
        // Points only fall with position, so the first place that works
        // is the limit. Past the field: only not finishing will do.
        let q = (1..=n)
            .find(|&q| self.beats(l, pl, r, Some(q)))
            .unwrap_or(n + 1);
        // The best place left for the rival is no limit.
        let best = if pl == Some(1) { 2 } else { 1 };
        (q > best).then_some(q)
    }

    /// One block per driver who could secure the title in this session,
    /// leader first.
    pub fn blocks(&self) -> Vec<Block> {
        self.by_points()
            .into_iter()
            .filter_map(|l| Some(self.block(l, self.needs(l)?)))
            .collect()
    }

    /// Entry indices, most points first.
    fn by_points(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.entries.len()).collect();
        order.sort_by(|&a, &b| self.entries[b].points.total_cmp(&self.entries[a].points));
        order
    }

    /// What each rival must do with `l` at `pl`.
    fn rival_conditions(&self, l: usize, pl: Option<u32>) -> Vec<Condition> {
        let mut conditions = Vec::new();
        for r in self.by_points() {
            let rival = &self.entries[r];
            if r == l {
                continue;
            }
            if let Some(q) = self.rival_limit(l, pl, r) {
                conditions.push(Condition {
                    tla: rival.tla.clone(),
                    pos: q,
                    own: false,
                    met: rival.pos.is_none_or(|c| c >= q),
                });
            }
        }
        conditions
    }

    /// The block for `l`, worked out for where they're running, or the
    /// nearest place that still does it.
    fn block(&self, l: usize, needs: Option<u32>) -> Block {
        let e = &self.entries[l];
        let pl = match needs {
            Some(max) => Some(e.pos.map_or(max, |cur| cur.min(max))),
            None => e.pos,
        };
        let rivals = self.rival_conditions(l, pl);
        // Where they finish matters unless scoring nothing asks no more
        // of the rivals.
        let own = pl.filter(|_| needs.is_some() || self.rival_conditions(l, None) != rivals);
        let mut conditions = Vec::new();
        if let Some(p) = own {
            conditions.push(Condition {
                tla: e.tla.clone(),
                pos: p,
                own: true,
                met: e.pos.is_some_and(|c| c <= p),
            });
        }
        conditions.extend(rivals);
        Block {
            num: e.num.clone(),
            tla: e.tla.clone(),
            secured: conditions.iter().all(|c| c.met),
            possible: true,
            conditions,
        }
    }

    /// The block for a driver who could secure the title earlier in the
    /// session and now can't.
    pub fn no_longer(&self, num: &str) -> Option<Block> {
        let e = self.entries.iter().find(|e| e.num == num)?;
        Some(Block {
            num: e.num.clone(),
            tla: e.tla.clone(),
            secured: false,
            possible: false,
            conditions: Vec::new(),
        })
    }
}

/// What a driver needs to secure the title in this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub num: String,
    pub tla: String,
    /// Secured if the session finished as it's running now.
    pub secured: bool,
    /// False once it can't happen in this session any more.
    pub possible: bool,
    pub conditions: Vec<Condition>,
}

/// "ANT pos 2 or above" (`own`) or "RUS pos 5 or below".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub tla: String,
    pub pos: u32,
    pub own: bool,
    /// True as they're running now.
    pub met: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(tla: &str, points: f64, wins: u32, pos: u32) -> Entry {
        Entry {
            num: tla.to_owned(),
            tla: tla.to_owned(),
            points,
            wins,
            pos: Some(pos),
        }
    }

    /// `entries` plus drivers on no points behind them, to 22 cars.
    fn field(mut entries: Vec<Entry>) -> Vec<Entry> {
        for i in entries.len()..22 {
            entries.push(entry(&format!("D{i:02}"), 0.0, 0, i as u32 + 1));
        }
        entries
    }

    /// Austin 2026: ANT 340 (8 wins), RUS 238 (3), HAM 229 (1), VER 221
    /// (2), five grands prix after it, no sprints.
    fn austin(order: &[&str]) -> Fight {
        let standings = [
            ("ANT", 340.0, 8),
            ("RUS", 238.0, 3),
            ("HAM", 229.0, 1),
            ("VER", 221.0, 2),
        ];
        let mut entries: Vec<Entry> = (0..22)
            .map(|i| entry(&format!("D{i:02}"), 50.0, 0, 0))
            .collect();
        for (i, (tla, pts, wins)) in standings.iter().enumerate() {
            entries[i] = entry(tla, *pts, *wins, 0);
        }
        // Running order: named drivers where given, the rest after them.
        let mut pos = 1;
        for tla in order {
            entries.iter_mut().find(|e| e.tla == *tla).unwrap().pos = Some(pos);
            pos += 1;
        }
        for e in entries.iter_mut().filter(|e| e.pos == Some(0)) {
            e.pos = Some(pos);
            pos += 1;
        }
        Fight::new(entries, &Rules::for_year(2026), false, &[false; 5])
    }

    fn lines(b: &Block) -> Vec<String> {
        b.conditions
            .iter()
            .map(|c| {
                let dir = match (c.own, c.pos) {
                    (true, 1) => "",
                    (true, _) => " or above",
                    (false, _) => " or below",
                };
                format!("{} {}{dir}{}", c.tla, c.pos, if c.met { " ✓" } else { "" })
            })
            .collect()
    }

    #[test]
    fn austin_needs_the_win() {
        let f = austin(&["RUS", "ANT", "HAM"]);
        let blocks = f.blocks();
        assert_eq!(blocks.len(), 1);
        let b = &blocks[0];
        assert_eq!(b.tla, "ANT");
        assert!(!b.secured);
        // P9 for RUS ties on points; ANT has the wins (9 to 8).
        assert_eq!(lines(b), ["ANT 1", "RUS 9 or below", "HAM 5 or below"]);
    }

    #[test]
    fn austin_secured_as_it_runs() {
        let mut order = vec!["ANT", "D10", "D11", "D12", "HAM"];
        order.extend(["D13", "D14", "D15", "RUS"]);
        let b = &austin(&order).blocks()[0];
        assert!(b.secured);
        assert_eq!(
            lines(b),
            ["ANT 1 ✓", "RUS 9 or below ✓", "HAM 5 or below ✓"]
        );
    }

    #[test]
    fn retired_leader_can_no_longer() {
        let mut f = austin(&["RUS", "HAM"]);
        f.entries[0].pos = None;
        assert!(f.blocks().is_empty());
        let b = f.no_longer("ANT").unwrap();
        assert!(!b.possible && !b.secured);
    }

    #[test]
    fn far_enough_ahead_anywhere_will_do() {
        let mut f = austin(&["RUS", "HAM", "ANT"]);
        f.entries[0].points = 400.0;
        let b = &f.blocks()[0];
        // RUS can't get within 125 even winning.
        assert_eq!(lines(b), Vec::<String>::new());
        assert!(b.secured);
    }

    #[test]
    fn equal_wins_isnt_secured() {
        // 25 ahead with one grand prix left: scoring nothing here, AAA can
        // be drawn level by BBB winning both, and they'd have 4 wins each.
        let entries = vec![entry("AAA", 100.0, 4, 11), entry("BBB", 75.0, 3, 12)];
        let f = Fight::new(field(entries), &Rules::for_year(2026), false, &[false]);
        assert_eq!(
            lines(&f.blocks()[0]),
            ["AAA 10 or above", "BBB 11 or below ✓"]
        );
        // With a win fewer for BBB, AAA has the tie-break: anywhere, as
        // long as BBB scores nothing.
        let entries = vec![entry("AAA", 100.0, 4, 11), entry("BBB", 75.0, 2, 12)];
        let f = Fight::new(field(entries), &Rules::for_year(2026), false, &[false]);
        assert_eq!(lines(&f.blocks()[0]), ["BBB 11 or below ✓"]);
    }

    #[test]
    fn abu_dhabi_2025_grid() {
        // Final round: NOR 408, VER 396, PIA 392; VER, NOR, PIA on the grid.
        let entries = vec![
            entry("NOR", 408.0, 7, 2),
            entry("VER", 396.0, 7, 1),
            entry("PIA", 392.0, 7, 3),
        ];
        let f = Fight::new(field(entries), &Rules::for_year(2025), false, &[]);
        let b = f.blocks();
        // From P2 nobody can catch NOR.
        assert_eq!(lines(&b[0]), ["NOR 2 or above ✓"]);
        assert!(b[0].secured);
        assert_eq!(lines(&b[1]), ["VER 1 ✓", "NOR 4 or below"]);
        assert_eq!(
            lines(&b[2]),
            ["PIA 2 or above", "NOR 10 or below", "VER 4 or below"]
        );
    }

    #[test]
    fn final_round_level() {
        let entries = vec![entry("AAA", 100.0, 3, 2), entry("BBB", 100.0, 3, 1)];
        let f = Fight::new(field(entries), &Rules::for_year(2026), false, &[]);
        let b = f.blocks();
        assert_eq!(b.len(), 2);
        assert_eq!(lines(&b[0]), ["AAA 2 or above ✓", "BBB 3 or below"]);
        assert!(!b[0].secured);
        assert_eq!(lines(&b[1]), ["BBB 1 ✓"]);
        assert!(b[1].secured);
    }

    #[test]
    fn sprint_leaves_its_grand_prix() {
        let f = Fight::new(Vec::new(), &Rules::for_year(2026), true, &[true, false]);
        assert_eq!(f.left, 25.0 * 3.0 + 8.0);
        assert_eq!(f.later_gps, 3);
        assert!(!f.gp);
        let f = Fight::new(Vec::new(), &Rules::for_year(2024), false, &[false]);
        assert_eq!(f.left, 26.0);
    }
}

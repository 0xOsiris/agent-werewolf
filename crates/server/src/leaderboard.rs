//! Win records per AI model, plus one row for all humans, built from the game summaries.

use std::{collections::BTreeMap, fs, io, path::Path};

use serde::{Deserialize, Serialize};
use tracing::warn;
use werewolf_protocol::Role;

use crate::game_log::GameSummary;

/// Rows with fewer games than this rank below the rest, so one lucky win can't top the table.
pub const MIN_GAMES: u32 = 3;
/// Newest summaries read at startup; older ones are left out so a huge log dir can't stall boot.
const MAX_FILES: usize = 5000;
/// Summaries are a few KB; anything far bigger is not one of ours.
const MAX_FILE_BYTES: u64 = 1 << 20;
const SUFFIX: &str = "-summary.json";

/// Who a row counts: one AI model, or every human together.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Who {
    Humans,
    Model(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Record {
    games: u32,
    wins: u32,
    wolf_games: u32,
    wolf_wins: u32,
}

/// One leaderboard line; the village record is the total minus the wolf record.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// The model id, or "Humans".
    pub name: String,
    pub human: bool,
    pub games: u32,
    pub wins: u32,
    pub wolf_games: u32,
    pub wolf_wins: u32,
}

#[derive(Default, Debug)]
pub struct Leaderboard {
    records: BTreeMap<Who, Record>,
}

/// The parts of a saved summary the leaderboard reads; unknown fields are ignored.
#[derive(Deserialize)]
struct SavedSummary {
    players: Vec<SavedPlayer>,
}

#[derive(Deserialize)]
struct SavedPlayer {
    model: Option<String>,
    #[serde(default)]
    human: bool,
    role: Role,
    won: bool,
}

impl Leaderboard {
    /// Reads the newest summaries in `dir`; a missing dir is an empty board, bad files are skipped.
    pub fn load(dir: &Path) -> Self {
        let mut board = Self::default();
        let mut paths = match summary_paths(dir) {
            Ok(paths) => paths,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return board,
            Err(err) => {
                warn!(dir = %dir.display(), %err, "can't list game summaries; the leaderboard starts empty");
                return board;
            }
        };
        // Names start with a unix timestamp, so reverse name order is newest first.
        paths.sort_unstable_by(|a, b| b.cmp(a));
        if paths.len() > MAX_FILES {
            warn!(
                found = paths.len(),
                kept = MAX_FILES,
                "too many game summaries; the leaderboard counts only the newest"
            );
            paths.truncate(MAX_FILES);
        }
        let mut skipped = 0;
        for path in &paths {
            match read_summary(path) {
                Some(summary) => {
                    for p in summary.players {
                        board.add(p.human, p.model.as_deref(), p.role, p.won);
                    }
                }
                None => skipped += 1,
            }
        }
        if skipped > 0 {
            warn!(
                dir = %dir.display(),
                skipped,
                read = paths.len() - skipped,
                "skipped unreadable game summaries in the leaderboard"
            );
        }
        board
    }

    /// Adds a finished game.
    pub fn record(&mut self, summary: &GameSummary) {
        for p in &summary.players {
            self.add(p.human, p.model.as_deref(), p.role, p.won);
        }
    }

    /// Rows by win rate, best first; rows under [`MIN_GAMES`] come last.
    pub fn ranked(&self) -> Vec<Row> {
        let mut rows: Vec<(&Who, &Record)> = self.records.iter().collect();
        rows.sort_by(|(a_who, a), (b_who, b)| {
            let qualified = |r: &Record| r.games >= MIN_GAMES;
            // Cross-multiplied so the comparison is exact; games is never 0 for a stored record.
            let rate = |r: &Record, other: &Record| u64::from(r.wins) * u64::from(other.games);
            qualified(b)
                .cmp(&qualified(a))
                .then(rate(b, a).cmp(&rate(a, b)))
                .then(b.wins.cmp(&a.wins))
                .then(a_who.cmp(b_who))
        });
        rows.into_iter()
            .map(|(who, r)| Row {
                name: match who {
                    Who::Humans => "Humans".to_owned(),
                    Who::Model(model) => model.clone(),
                },
                human: *who == Who::Humans,
                games: r.games,
                wins: r.wins,
                wolf_games: r.wolf_games,
                wolf_wins: r.wolf_wins,
            })
            .collect()
    }

    fn add(&mut self, human: bool, model: Option<&str>, role: Role, won: bool) {
        let who = match (human, model) {
            (true, _) => Who::Humans,
            (false, Some(model)) => Who::Model(model.to_owned()),
            // Mock agents have no model, and there is nothing to rank.
            (false, None) => return,
        };
        let record = self.records.entry(who).or_default();
        let wolf = role == Role::Werewolf;
        record.games += 1;
        record.wins += u32::from(won);
        record.wolf_games += u32::from(wolf);
        record.wolf_wins += u32::from(wolf && won);
    }
}

fn summary_paths(dir: &Path) -> io::Result<Vec<std::path::PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(SUFFIX))
        {
            paths.push(path);
        }
    }
    Ok(paths)
}

/// `None` if the file is missing, oversized, or not a summary.
fn read_summary(path: &Path) -> Option<SavedSummary> {
    let size = fs::metadata(path).ok()?.len();
    if size > MAX_FILE_BYTES {
        return None;
    }
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game_log::PlayerSummary;
    use werewolf_protocol::Team;

    fn player(
        name: &str,
        model: Option<&str>,
        human: bool,
        role: Role,
        won: bool,
    ) -> PlayerSummary {
        PlayerSummary {
            id: 0,
            name: name.into(),
            model: model.map(Into::into),
            human,
            role,
            survived: won,
            died_on: None,
            timeouts: 0,
            invalid_actions: 0,
            shot: None,
            lover_of: None,
            won,
        }
    }

    fn game(winner: Team, players: Vec<PlayerSummary>) -> GameSummary {
        GameSummary {
            seed: 1,
            winner: Some(winner),
            days_played: 2,
            players,
        }
    }

    fn row(board: &Leaderboard, name: &str) -> Row {
        board.ranked().into_iter().find(|r| r.name == name).unwrap()
    }

    #[test]
    fn counts_models_and_humans_and_skips_mock_agents() {
        let mut board = Leaderboard::default();
        board.record(&game(
            Team::Village,
            vec![
                player("Sherlock", Some("a/opus"), false, Role::Seer, true),
                player("Watson", Some("a/sonnet"), false, Role::Werewolf, false),
                player("ann", None, true, Role::Villager, true),
                player("bob", Some("typed-by-a-human"), true, Role::Werewolf, false),
                player("mock-1", None, false, Role::Doctor, true),
            ],
        ));
        let rows = board.ranked();
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!(
            row(&board, "Humans"),
            Row {
                name: "Humans".into(),
                human: true,
                games: 2,
                wins: 1,
                wolf_games: 1,
                wolf_wins: 0,
            }
        );
        assert_eq!(row(&board, "a/sonnet").wolf_games, 1);
        assert!(rows.iter().all(|r| !r.name.starts_with("mock")));
    }

    #[test]
    fn ranks_by_win_rate_with_small_samples_last() {
        let mut board = Leaderboard::default();
        let play = |board: &mut Leaderboard, model: &str, won: bool| {
            board.record(&game(
                Team::Village,
                vec![player("x", Some(model), false, Role::Villager, won)],
            ));
        };
        // "lucky" wins its only game; "steady" wins 3 of 4; "busy" wins 3 of 6.
        play(&mut board, "lucky", true);
        for won in [true, true, true, false] {
            play(&mut board, "steady", won);
        }
        for won in [true, false, true, false, true, false] {
            play(&mut board, "busy", won);
        }
        let names: Vec<String> = board.ranked().into_iter().map(|r| r.name).collect();
        assert_eq!(names, ["steady", "busy", "lucky"]);
    }

    #[test]
    fn loads_saved_summaries_and_skips_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        let summary = game(
            Team::Wolves,
            vec![
                player("Wednesday", Some("d/flash"), false, Role::Werewolf, true),
                player("osiris", None, true, Role::Villager, false),
            ],
        );
        let json = serde_json::to_vec(&summary).unwrap();
        fs::write(dir.path().join("100-1-summary.json"), &json).unwrap();
        fs::write(dir.path().join("200-2-summary.json"), &json).unwrap();
        fs::write(dir.path().join("300-3-summary.json"), "{ not json").unwrap();
        fs::write(dir.path().join("300-3.jsonl"), "events, not a summary").unwrap();

        let board = Leaderboard::load(dir.path());
        assert_eq!(
            row(&board, "d/flash"),
            Row {
                name: "d/flash".into(),
                human: false,
                games: 2,
                wins: 2,
                wolf_games: 2,
                wolf_wins: 2,
            }
        );
        assert_eq!(row(&board, "Humans").games, 2);
    }

    #[test]
    fn a_missing_dir_is_an_empty_board() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            Leaderboard::load(&dir.path().join("nope"))
                .ranked()
                .is_empty()
        );
    }
}

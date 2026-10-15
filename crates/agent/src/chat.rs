//! Open chat windows: which messages belong to the window a chat turn is in.

use werewolf_protocol::{PlayerId, PlayerView, PrivateEvent, PublicEvent, TurnKind};

/// Public events since the current phase began.
pub fn this_phase(log: &[PublicEvent]) -> &[PublicEvent] {
    after_last(log, |e| matches!(e, PublicEvent::PhaseStarted { .. }))
}

/// Tonight's wolf chat; the private log has no phase markers, but each night's chat ends with the kill vote.
pub fn tonight(log: &[PrivateEvent]) -> &[PrivateEvent] {
    after_last(log, |e| matches!(e, PrivateEvent::WolfVoted { .. }))
}

/// Day messages in this phase that `you` haven't responded to yet.
pub fn new_public(log: &[PublicEvent], you: PlayerId) -> &[PublicEvent] {
    after_last(this_phase(log), |e| said_by(e, you))
}

/// Tonight's wolf chat that `you` haven't responded to yet.
pub fn new_wolf_chat(log: &[PrivateEvent], you: PlayerId) -> &[PrivateEvent] {
    after_last(tonight(log), |e| wolf_said_by(e, you))
}

/// How many messages this player has sent in the current window of a chat turn.
pub fn own_messages(view: &PlayerView, kind: TurnKind) -> usize {
    match kind {
        TurnKind::DaySpeak => this_phase(&view.public_log)
            .iter()
            .filter(|e| said_by(e, view.you))
            .count(),
        TurnKind::WolfChat => tonight(&view.private_log)
            .iter()
            .filter(|e| wolf_said_by(e, view.you))
            .count(),
        _ => 0,
    }
}

fn said_by(event: &PublicEvent, you: PlayerId) -> bool {
    matches!(event, PublicEvent::Spoke { player, .. } if *player == you)
}

fn wolf_said_by(event: &PrivateEvent, you: PlayerId) -> bool {
    matches!(event, PrivateEvent::WolfSpoke { player, .. } if *player == you)
}

fn after_last<T>(events: &[T], is_marker: impl Fn(&T) -> bool) -> &[T] {
    let start = events.iter().rposition(is_marker).map_or(0, |i| i + 1);
    &events[start..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use werewolf_protocol::Phase;

    const ME: PlayerId = 1;
    const BOB: PlayerId = 2;

    fn said(player: PlayerId, text: &str) -> PublicEvent {
        PublicEvent::Spoke {
            player,
            text: text.into(),
        }
    }

    fn wolf_said(player: PlayerId, text: &str) -> PrivateEvent {
        PrivateEvent::WolfSpoke {
            player,
            text: text.into(),
        }
    }

    fn texts(events: &[PublicEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                PublicEvent::Spoke { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn new_messages_start_after_phase_and_own_last_message() {
        let day = |n| PublicEvent::PhaseStarted {
            phase: Phase::Day { n },
        };
        let mut log = vec![
            day(1),
            said(ME, "old"),
            said(BOB, "yesterday"),
            day(2),
            said(BOB, "a"),
        ];
        assert_eq!(texts(new_public(&log, ME)), ["a"]);

        log.extend([said(ME, "b"), said(BOB, "c")]);
        assert_eq!(texts(new_public(&log, ME)), ["c"]);
    }

    #[test]
    fn wolf_chat_starts_after_last_kill_vote() {
        let vote = PrivateEvent::WolfVoted {
            voter: BOB,
            target: None,
        };
        let mut log = vec![wolf_said(ME, "night 1"), vote, wolf_said(BOB, "x")];
        assert_eq!(new_wolf_chat(&log, ME).len(), 1);
        log.push(wolf_said(ME, "y"));
        assert!(new_wolf_chat(&log, ME).is_empty());
        assert_eq!(tonight(&log).len(), 2);
    }

    #[test]
    fn own_messages_counts_only_the_current_window() {
        let view = PlayerView {
            you: ME,
            role: werewolf_protocol::Role::Werewolf,
            phase: Phase::Day { n: 2 },
            players: vec![],
            wolf_team: vec![BOB],
            public_log: vec![
                said(ME, "day 1"),
                PublicEvent::PhaseStarted {
                    phase: Phase::Day { n: 2 },
                },
                said(ME, "a"),
                said(BOB, "b"),
                said(ME, "c"),
            ],
            private_log: vec![wolf_said(ME, "night 1")],
        };
        assert_eq!(own_messages(&view, TurnKind::DaySpeak), 2);
        assert_eq!(own_messages(&view, TurnKind::WolfChat), 1);
        assert_eq!(own_messages(&view, TurnKind::DayVote), 0);
    }
}

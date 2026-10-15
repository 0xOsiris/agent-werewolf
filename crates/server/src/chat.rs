//! Turn-issuing rules for one open chat window. Pure: callers pass the clock in.

use std::time::Duration;

use rand::{RngExt, SeedableRng, rngs::StdRng};
use tokio::time::Instant;
use werewolf_protocol::PlayerId;

/// Per-player chat limits.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Minimum gap between one player's answers.
    pub cooldown: Duration,
    /// Messages one player may post per window.
    pub max_messages: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct ChatRules {
    pub human: Limits,
    pub ai: Limits,
    /// Agents wait an extra uniform 0..jitter before each turn, so they don't all pile in at once.
    pub ai_jitter: Duration,
    /// Close early once nobody has posted for this long and everyone has answered.
    pub quiet: Duration,
}

impl ChatRules {
    fn limits(&self, human: bool) -> Limits {
        if human { self.human } else { self.ai }
    }
}

pub struct ChatWindow {
    rules: ChatRules,
    rng: StdRng,
    opened: Instant,
    ends: Instant,
    /// Last post, or the opening if nobody has posted yet.
    last_post: Instant,
    members: Vec<Member>,
}

struct Member {
    player: PlayerId,
    human: bool,
    holding: bool,
    last_answer: Option<Instant>,
    /// First post by someone else since `last_answer`.
    reply_at: Option<Instant>,
    /// Extra wait before this agent's next turn, drawn at opening and at each answer.
    jitter: Duration,
    messages: u32,
}

impl Member {
    /// When this member may next get a turn; `None` while holding one, capped, or (for agents)
    /// waiting for someone else to post.
    fn ready_at(&self, rules: &ChatRules, opened: Instant) -> Option<Instant> {
        let limits = rules.limits(self.human);
        if self.holding || self.messages >= limits.max_messages {
            return None;
        }
        match (self.last_answer, self.human) {
            (None, true) => Some(opened),
            (None, false) => Some(opened + self.jitter),
            (Some(at), true) => Some(at + limits.cooldown),
            // Agents wait for a reply so they don't monologue.
            (Some(at), false) => self
                .reply_at
                .map(|reply| (at + limits.cooldown).max(reply) + self.jitter),
        }
    }
}

impl ChatWindow {
    /// Opens a window for `members` (seat, is human) that runs for `length`. `seed` drives the
    /// agents' jitter.
    pub fn open(
        members: impl IntoIterator<Item = (PlayerId, bool)>,
        rules: ChatRules,
        now: Instant,
        length: Duration,
        seed: u64,
    ) -> Self {
        let mut window = Self {
            rules,
            rng: StdRng::seed_from_u64(seed),
            opened: now,
            ends: now + length,
            last_post: now,
            members: Vec::new(),
        };
        for (player, human) in members {
            let jitter = window.draw_jitter(human);
            window.members.push(Member {
                player,
                human,
                holding: false,
                last_answer: None,
                reply_at: None,
                jitter,
                messages: 0,
            });
        }
        window
    }

    pub fn ends(&self) -> Instant {
        self.ends
    }

    /// Drops members `keep` rejects, such as players the sheriff just shot.
    pub fn retain(&mut self, keep: impl Fn(PlayerId) -> bool) {
        self.members.retain(|m| keep(m.player));
    }

    /// Records an answer; true if it is published as a post (non-empty and under the cap).
    pub fn answer(&mut self, player: PlayerId, empty: bool, now: Instant) -> bool {
        let Some(index) = self.members.iter().position(|m| m.player == player) else {
            return false;
        };
        let human = self.members[index].human;
        let jitter = self.draw_jitter(human);
        let limits = self.rules.limits(human);
        let member = &mut self.members[index];
        let posted = !empty && member.messages < limits.max_messages;
        member.holding = false;
        member.last_answer = Some(now);
        member.reply_at = None;
        member.jitter = jitter;
        if posted {
            member.messages += 1;
            self.wake_others(Some(index), now);
        }
        posted
    }

    /// Treats a public event, such as the sheriff's shot, as a post that anyone may answer.
    pub fn stir(&mut self, now: Instant) {
        self.wake_others(None, now);
    }

    /// Restarts the quiet timer and gives every waiting member but `poster` something to reply to.
    fn wake_others(&mut self, poster: Option<usize>, now: Instant) {
        self.last_post = now;
        for (i, other) in self.members.iter_mut().enumerate() {
            if Some(i) != poster && other.last_answer.is_some() && other.reply_at.is_none() {
                other.reply_at = Some(now);
            }
        }
    }

    /// Players due a new turn at `now`; they count as holding one from here on.
    pub fn due(&mut self, now: Instant) -> Vec<PlayerId> {
        let (rules, opened) = (self.rules, self.opened);
        self.members
            .iter_mut()
            .filter(|m| m.ready_at(&rules, opened).is_some_and(|at| at <= now))
            .map(|m| {
                m.holding = true;
                m.player
            })
            .collect()
    }

    /// A pending (jittered) turn is not a post, so it never holds off the quiet close.
    pub fn should_close(&self, now: Instant) -> bool {
        now >= self.ends || self.quiet_at().is_some_and(|at| now >= at)
    }

    /// The next time the window needs attention if no answer arrives first.
    pub fn next_wake(&self) -> Instant {
        self.next_turn()
            .into_iter()
            .chain(self.quiet_at())
            .fold(self.ends, Instant::min)
    }

    /// When the next member becomes due a turn, if any will.
    pub fn next_turn(&self) -> Option<Instant> {
        self.members
            .iter()
            .filter_map(|m| m.ready_at(&self.rules, self.opened))
            .min()
    }

    /// When the window goes quiet, once every member has answered at least once.
    fn quiet_at(&self) -> Option<Instant> {
        self.members
            .iter()
            .all(|m| m.last_answer.is_some())
            .then(|| self.last_post + self.rules.quiet)
    }

    fn draw_jitter(&mut self, human: bool) -> Duration {
        if human || self.rules.ai_jitter.is_zero() {
            Duration::ZERO
        } else {
            self.rng.random_range(Duration::ZERO..self.rules.ai_jitter)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: ChatRules = ChatRules {
        human: Limits {
            cooldown: Duration::from_secs(8),
            max_messages: 4,
        },
        ai: Limits {
            cooldown: Duration::from_secs(15),
            max_messages: 2,
        },
        ai_jitter: Duration::from_secs(10),
        quiet: Duration::from_secs(20),
    };
    const JITTER: Duration = RULES.ai_jitter;

    fn secs(t0: Instant, s: u64) -> Instant {
        t0 + Duration::from_secs(s)
    }

    /// Seats 0 and 1 are agents, seat 2 is a human; 120s window.
    fn window(t0: Instant, rules: ChatRules, seed: u64) -> ChatWindow {
        ChatWindow::open(
            [(0, false), (1, false), (2, true)],
            rules,
            t0,
            Duration::from_secs(120),
            seed,
        )
    }

    fn ready(chat: &ChatWindow, player: PlayerId) -> Option<Instant> {
        let member = chat.members.iter().find(|m| m.player == player)?;
        member.ready_at(&chat.rules, chat.opened)
    }

    /// Opens a window and hands every first turn out.
    fn started(t0: Instant) -> ChatWindow {
        let mut chat = window(t0, RULES, 7);
        assert_eq!(chat.due(secs(t0, 10)), vec![0, 1, 2]);
        chat
    }

    #[test]
    fn humans_start_at_once_and_agents_after_jitter() {
        let t0 = Instant::now();
        for seed in 0..200 {
            let chat = window(t0, RULES, seed);
            assert_eq!(ready(&chat, 2), Some(t0), "humans don't wait");
            for agent in [0, 1] {
                let at = ready(&chat, agent).unwrap();
                assert!(at >= t0 && at < t0 + JITTER, "seed {seed}: {:?}", at - t0);
            }
        }
        let mut chat = window(t0, RULES, 1);
        assert_eq!(chat.due(t0), vec![2]);
        assert_eq!(chat.due(secs(t0, 10)), vec![0, 1]);
    }

    #[test]
    fn jitter_is_deterministic_per_seed() {
        let t0 = Instant::now();
        let times = |seed| {
            let mut chat = window(t0, RULES, seed);
            chat.due(secs(t0, 10));
            chat.answer(0, false, secs(t0, 10));
            chat.answer(1, false, secs(t0, 11));
            [ready(&chat, 0), ready(&chat, 1)]
        };
        assert_eq!(times(42), times(42));
        assert!(
            (0..20).any(|seed| times(seed) != times(42)),
            "the seed matters"
        );
    }

    #[test]
    fn agents_use_the_longer_cooldown_and_wait_for_a_reply() {
        let t0 = Instant::now();
        let mut chat = started(t0);
        chat.answer(0, false, secs(t0, 10));
        assert_eq!(ready(&chat, 0), None, "no one else has posted yet");
        chat.answer(2, false, secs(t0, 12));
        let at = ready(&chat, 0).unwrap();
        assert!(
            at >= secs(t0, 25) && at < secs(t0, 25) + JITTER,
            "{:?}",
            at - t0
        );
        assert!(chat.due(secs(t0, 24)).iter().all(|&p| p != 0));
    }

    #[test]
    fn agents_reply_late_by_up_to_the_jitter() {
        let t0 = Instant::now();
        let mut chat = started(t0);
        chat.answer(0, false, secs(t0, 10));
        // The reply comes after the cooldown, so eligibility starts at the reply.
        chat.answer(1, false, secs(t0, 40));
        let at = ready(&chat, 0).unwrap();
        assert!(
            at >= secs(t0, 40) && at < secs(t0, 40) + JITTER,
            "{:?}",
            at - t0
        );
    }

    #[test]
    fn humans_only_wait_for_their_cooldown() {
        let t0 = Instant::now();
        let mut chat = started(t0);
        chat.answer(2, false, secs(t0, 10));
        assert_eq!(ready(&chat, 2), Some(secs(t0, 18)));
        assert!(chat.due(secs(t0, 17)).is_empty());
        assert_eq!(chat.due(secs(t0, 18)), vec![2]);
    }

    #[test]
    fn agents_and_humans_have_separate_caps() {
        let t0 = Instant::now();
        let mut chat = started(t0);
        let mut now = secs(t0, 10);
        for _ in 0..2 {
            assert!(chat.answer(0, false, now));
            assert!(chat.answer(2, false, now + Duration::from_secs(1)));
            now += Duration::from_secs(60);
            chat.due(now);
        }
        assert_eq!(ready(&chat, 0), None, "agents stop at 2");
        assert!(!chat.answer(0, false, now), "over-cap answers don't post");
        for _ in 0..2 {
            assert!(chat.answer(2, false, now));
            now += Duration::from_secs(60);
            chat.due(now);
        }
        assert_eq!(ready(&chat, 2), None, "humans stop at 4");
    }

    #[test]
    fn passing_counts_as_answering_but_not_posting() {
        let t0 = Instant::now();
        let mut chat = started(t0);
        assert!(!chat.answer(0, true, secs(t0, 10)));
        assert_eq!(ready(&chat, 0), None);
        chat.answer(1, false, secs(t0, 11));
        assert!(ready(&chat, 0).is_some(), "a reply re-opens the floor");
    }

    #[test]
    fn quiet_close_ignores_pending_jittered_turns() {
        let t0 = Instant::now();
        let mut chat = started(t0);
        chat.answer(1, true, secs(t0, 10));
        chat.answer(0, false, secs(t0, 11));
        assert!(
            !chat.should_close(secs(t0, 60)),
            "the human hasn't answered"
        );
        chat.answer(2, true, secs(t0, 61));
        // Agent 1 has a jittered turn coming, but nobody has posted since 11s.
        assert!(ready(&chat, 1).is_some());
        assert!(chat.should_close(secs(t0, 61)));
    }

    #[test]
    fn a_stir_wakes_everyone_without_using_a_message() {
        let t0 = Instant::now();
        let mut chat = started(t0);
        chat.answer(0, false, secs(t0, 10));
        chat.answer(1, true, secs(t0, 11));
        chat.answer(2, true, secs(t0, 12));
        assert_eq!(ready(&chat, 0), None, "nobody has replied to seat 0");
        chat.stir(secs(t0, 25));
        let at = ready(&chat, 0).unwrap();
        assert!(
            at >= secs(t0, 25) && at < secs(t0, 25) + JITTER,
            "{:?}",
            at - t0
        );
        assert!(
            !chat.should_close(secs(t0, 44)),
            "the quiet timer restarted"
        );
        assert!(chat.should_close(secs(t0, 45)));
        assert_eq!(chat.members[0].messages, 1);
    }

    #[test]
    fn retain_drops_the_dead_so_they_never_hold_the_window_open() {
        let t0 = Instant::now();
        let mut chat = started(t0);
        chat.answer(0, false, secs(t0, 10));
        chat.answer(2, true, secs(t0, 11));
        assert!(!chat.should_close(secs(t0, 60)), "seat 1 hasn't answered");
        chat.retain(|p| p != 1);
        assert!(chat.should_close(secs(t0, 60)));
        assert!(
            !chat.answer(1, false, secs(t0, 61)),
            "a dropped member can't post"
        );
        assert!(chat.due(secs(t0, 100)).iter().all(|&p| p != 1));
    }

    #[test]
    fn closes_when_time_is_up() {
        let t0 = Instant::now();
        let chat = started(t0);
        assert!(!chat.should_close(secs(t0, 119)));
        assert!(chat.should_close(secs(t0, 120)));
        assert_eq!(chat.next_wake(), secs(t0, 120));
    }

    #[test]
    fn next_turn_ignores_the_quiet_close_and_the_end() {
        let t0 = Instant::now();
        let mut chat = started(t0);
        assert_eq!(chat.next_turn(), None, "everyone holds a turn");
        chat.answer(2, false, secs(t0, 10));
        assert_eq!(chat.next_turn(), Some(secs(t0, 18)));
        for agent in [0, 1] {
            chat.answer(agent, true, secs(t0, 11));
        }
        chat.due(secs(t0, 18));
        // All answered and quiet since 10s: due to close at 30s, but no turn is coming.
        assert_eq!(chat.next_wake(), secs(t0, 30));
        assert_eq!(chat.next_turn(), None);
    }

    #[test]
    fn zero_jitter_starts_everyone_at_once() {
        let t0 = Instant::now();
        let rules = ChatRules {
            ai_jitter: Duration::ZERO,
            ..RULES
        };
        let mut chat = window(t0, rules, 3);
        assert_eq!(chat.due(t0), vec![0, 1, 2]);
    }
}

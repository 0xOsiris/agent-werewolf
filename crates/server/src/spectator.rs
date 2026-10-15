//! Events streamed to spectators and players, and appended to the game log.

use std::{borrow::Cow, sync::Arc};

use serde::Serialize;
use werewolf_protocol::{
    Phase, PlayerId, PrivateEvent, PublicEvent, PublicPlayer, Role, Team, TurnKind,
};

use crate::{
    engine::{Audience, GameEvent},
    settings::SettingsBody,
};

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SpectatorEvent {
    /// Full state; sent on connect and after a lagged subscriber.
    Snapshot {
        phase: Phase,
        winner: Option<Team>,
        settings: SettingsBody,
        players: Vec<PublicPlayer>,
        roles: Vec<RoleEntry>,
        /// Every `public`, `private`, and `reasoning` event so far, in order.
        events: Vec<SpectatorEvent>,
        /// Pending turns other than chat; chat is described by `chat`.
        pending: Vec<PendingTurn>,
        /// The open chat window, if any.
        chat: Option<ChatState>,
    },
    PlayerJoined {
        player: PublicPlayer,
    },
    Roles {
        roles: Vec<RoleEntry>,
    },
    Public {
        event: PublicEvent,
    },
    Private {
        audience: Audience,
        event: PrivateEvent,
    },
    TurnStarted {
        player: PlayerId,
        kind: TurnKind,
        deadline_ms: u64,
    },
    TurnEnded {
        player: PlayerId,
        kind: TurnKind,
        timed_out: bool,
    },
    Reasoning {
        player: PlayerId,
        kind: TurnKind,
        text: String,
    },
    /// A chat window opened (`ends_in_ms` = its length) or closed (`ends_in_ms` = 0).
    ChatWindow {
        kind: TurnKind,
        open: bool,
        ends_in_ms: u64,
    },
    /// The host changed the lobby settings.
    Settings {
        settings: SettingsBody,
    },
    Reset,
}

impl SpectatorEvent {
    /// Whether the event belongs in the snapshot's feed.
    pub fn is_feed(&self) -> bool {
        matches!(
            self,
            Self::Public { .. } | Self::Private { .. } | Self::Reasoning { .. }
        )
    }

    /// The event as `viewer` may see it, or `None` if it is hidden. `roles` is indexed by seat;
    /// once the game is `over`, every role is public.
    pub fn filtered(&self, viewer: Viewer, roles: &[Role], over: bool) -> Option<Cow<'_, Self>> {
        if viewer == Viewer::Spectator {
            return Some(Cow::Borrowed(self));
        }
        let visible = match self {
            Self::PlayerJoined { .. }
            | Self::Public { .. }
            | Self::Settings { .. }
            | Self::Reset => true,
            Self::Private { audience, .. } => {
                viewer.role(roles).and_then(Audience::of) == Some(*audience)
            }
            Self::TurnStarted { player, kind, .. } | Self::TurnEnded { player, kind, .. } => {
                viewer.sees_turn(*player, *kind, roles)
            }
            Self::ChatWindow { kind, .. } => viewer.sees_chat(*kind, roles),
            Self::Roles { .. } if over => true,
            Self::Roles { roles: entries } if matches!(viewer, Viewer::Player { .. }) => {
                return Some(Cow::Owned(Self::Roles {
                    roles: viewer.visible_roles(entries, roles, over),
                }));
            }
            // Snapshots are built per viewer and never broadcast.
            Self::Roles { .. } | Self::Reasoning { .. } | Self::Snapshot { .. } => false,
        };
        visible.then_some(Cow::Borrowed(self))
    }
}

impl From<GameEvent> for SpectatorEvent {
    fn from(event: GameEvent) -> Self {
        match event {
            GameEvent::Public(event) => Self::Public { event },
            GameEvent::Private(audience, event) => Self::Private { audience, event },
        }
    }
}

/// Who is watching, which decides what they may see.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Viewer {
    /// Holds the spectator key: sees everything.
    Spectator,
    /// No token: public information only.
    Audience,
    Player {
        id: PlayerId,
    },
}

impl Viewer {
    fn role(self, roles: &[Role]) -> Option<Role> {
        match self {
            Self::Player { id } => roles.get(id as usize).copied(),
            Self::Spectator | Self::Audience => None,
        }
    }

    fn is_wolf(self, roles: &[Role]) -> bool {
        self.role(roles) == Some(Role::Werewolf)
    }

    /// Day turns are public; night turns only show to their holder and, for wolf turns, the pack.
    pub fn sees_turn(self, player: PlayerId, kind: TurnKind, roles: &[Role]) -> bool {
        match self {
            Self::Spectator => true,
            _ if matches!(kind, TurnKind::DaySpeak | TurnKind::DayVote) => true,
            Self::Audience => false,
            Self::Player { id } => {
                player == id
                    || (matches!(kind, TurnKind::WolfChat | TurnKind::WolfKill)
                        && self.is_wolf(roles))
            }
        }
    }

    /// Day discussion is public; wolf chat only shows to the pack.
    pub fn sees_chat(self, kind: TurnKind, roles: &[Role]) -> bool {
        self == Self::Spectator || kind != TurnKind::WolfChat || self.is_wolf(roles)
    }

    /// Your own role, plus the pack's if you are a wolf; everyone's once the game is over.
    pub fn visible_roles(
        self,
        entries: &[RoleEntry],
        roles: &[Role],
        over: bool,
    ) -> Vec<RoleEntry> {
        entries
            .iter()
            .filter(|e| match self {
                Self::Spectator => true,
                Self::Audience => over,
                Self::Player { id } => {
                    over || e.player == id || (e.role == Role::Werewolf && self.is_wolf(roles))
                }
            })
            .cloned()
            .collect()
    }
}

/// A published event with its JSON, serialized once for every unfiltered subscriber.
#[derive(Debug)]
pub struct Broadcast {
    pub event: SpectatorEvent,
    pub json: Arc<str>,
}

#[derive(Serialize, Clone, Debug)]
pub struct RoleEntry {
    pub player: PlayerId,
    pub role: Role,
}

#[derive(Serialize, Clone, Debug)]
pub struct ChatState {
    pub kind: TurnKind,
    pub ends_in_ms: u64,
}

#[derive(Serialize, Clone, Debug)]
pub struct PendingTurn {
    pub player: PlayerId,
    pub kind: TurnKind,
    pub deadline_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROLES: [Role; 5] = [
        Role::Werewolf,
        Role::Werewolf,
        Role::Seer,
        Role::Doctor,
        Role::Villager,
    ];

    fn sees(viewer: Viewer, event: &SpectatorEvent) -> bool {
        event.filtered(viewer, &ROLES, false).is_some()
    }

    fn private(audience: Audience) -> SpectatorEvent {
        let event = match audience {
            Audience::Wolves => PrivateEvent::WolfSpoke {
                player: 0,
                text: "eat 4".into(),
            },
            Audience::Seer => PrivateEvent::Inspected {
                target: 0,
                is_werewolf: true,
            },
            Audience::Doctor => PrivateEvent::Protected { target: 4 },
        };
        SpectatorEvent::Private { audience, event }
    }

    fn turn(player: PlayerId, kind: TurnKind) -> SpectatorEvent {
        SpectatorEvent::TurnStarted {
            player,
            kind,
            deadline_ms: 1,
        }
    }

    #[test]
    fn private_events_reach_only_their_audience() {
        let wolf = Viewer::Player { id: 1 };
        let seer = Viewer::Player { id: 2 };
        let doctor = Viewer::Player { id: 3 };
        let villager = Viewer::Player { id: 4 };
        assert!(sees(wolf, &private(Audience::Wolves)));
        assert!(!sees(wolf, &private(Audience::Seer)));
        assert!(!sees(wolf, &private(Audience::Doctor)));
        assert!(sees(seer, &private(Audience::Seer)));
        assert!(!sees(seer, &private(Audience::Wolves)));
        assert!(sees(doctor, &private(Audience::Doctor)));
        for audience in [Audience::Wolves, Audience::Seer, Audience::Doctor] {
            assert!(!sees(villager, &private(audience)));
            assert!(!sees(Viewer::Audience, &private(audience)));
        }
    }

    #[test]
    fn night_turns_stay_hidden() {
        let wolf = Viewer::Player { id: 1 };
        let villager = Viewer::Player { id: 4 };
        assert!(sees(wolf, &turn(0, TurnKind::WolfKill)));
        assert!(!sees(wolf, &turn(2, TurnKind::SeerInspect)));
        assert!(!sees(wolf, &turn(3, TurnKind::DoctorProtect)));
        assert!(sees(
            Viewer::Player { id: 2 },
            &turn(2, TurnKind::SeerInspect)
        ));
        assert!(!sees(villager, &turn(0, TurnKind::WolfChat)));
        assert!(!sees(Viewer::Audience, &turn(3, TurnKind::DoctorProtect)));
        assert!(sees(Viewer::Audience, &turn(3, TurnKind::DayVote)));
    }

    #[test]
    fn wolf_chat_window_shows_only_to_wolves() {
        let window = |kind| SpectatorEvent::ChatWindow {
            kind,
            open: true,
            ends_in_ms: 40_000,
        };
        assert!(sees(Viewer::Player { id: 0 }, &window(TurnKind::WolfChat)));
        assert!(sees(Viewer::Spectator, &window(TurnKind::WolfChat)));
        for viewer in [
            Viewer::Player { id: 2 },
            Viewer::Player { id: 4 },
            Viewer::Audience,
        ] {
            assert!(!sees(viewer, &window(TurnKind::WolfChat)));
            assert!(sees(viewer, &window(TurnKind::DaySpeak)));
        }
    }

    #[test]
    fn roles_and_reasoning_are_filtered() {
        let roles = SpectatorEvent::Roles {
            roles: (0..)
                .zip(ROLES)
                .map(|(player, role)| RoleEntry { player, role })
                .collect(),
        };
        let shown = |viewer, over| match roles.filtered(viewer, &ROLES, over).as_deref() {
            Some(SpectatorEvent::Roles { roles }) => roles.iter().map(|r| r.player).collect(),
            _ => Vec::new(),
        };
        assert_eq!(shown(Viewer::Player { id: 0 }, false), vec![0, 1]);
        assert_eq!(shown(Viewer::Player { id: 2 }, false), vec![2]);
        assert_eq!(shown(Viewer::Audience, false), Vec::<PlayerId>::new());
        assert_eq!(shown(Viewer::Audience, true), vec![0, 1, 2, 3, 4]);
        assert_eq!(shown(Viewer::Player { id: 4 }, true), vec![0, 1, 2, 3, 4]);
        let reasoning = SpectatorEvent::Reasoning {
            player: 0,
            kind: TurnKind::WolfChat,
            text: "lie".into(),
        };
        assert!(!sees(Viewer::Player { id: 0 }, &reasoning));
        assert!(sees(Viewer::Spectator, &reasoning));
    }
}

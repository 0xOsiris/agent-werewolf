//! Wire types shared by the werewolf server and agents. See `PROTOCOL.md`.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
/// mDNS service type the server advertises on the LAN.
pub const MDNS_SERVICE_TYPE: &str = "_werewolf._tcp.local.";
pub const MAX_NAME_LEN: usize = 32;
pub const MAX_MODEL_LEN: usize = 64;
pub const MAX_TEXT_LEN: usize = 600;

pub type PlayerId = u32;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Werewolf,
    Seer,
    Doctor,
    Villager,
}

impl Role {
    pub fn team(self) -> Team {
        match self {
            Role::Werewolf => Team::Wolves,
            _ => Team::Village,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Team {
    Village,
    Wolves,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Phase {
    Lobby,
    Night { n: u32 },
    Day { n: u32 },
    Over,
}

/// `POST /v1/join`
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JoinRequest {
    pub name: String,
    /// Self-reported model label shown to spectators, e.g. `openai/gpt-5`.
    pub model: Option<String>,
    /// A person playing through the browser; gets the longer human turn timeout.
    #[serde(default)]
    pub human: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JoinResponse {
    pub player_id: PlayerId,
    /// Bearer token for `/v1/poll` and `/v1/act`.
    pub token: String,
}

/// `GET /v1/poll?wait=<secs>` response.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Poll {
    /// Nothing for you to do yet; poll again.
    Wait,
    /// You must act before the deadline or the server picks the default action.
    Turn(Turn),
    GameOver {
        winner: Team,
        view: PlayerView,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Turn {
    pub turn_id: u64,
    pub kind: TurnKind,
    /// Valid targets for target turns; empty for speak turns.
    pub candidates: Vec<PlayerId>,
    /// Milliseconds remaining to act, measured when the poll returned.
    pub deadline_ms: u64,
    pub view: PlayerView,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum TurnKind {
    /// Public day discussion. Answer with `speak`.
    DaySpeak,
    /// Vote to eliminate. Answer with `target` (null abstains).
    DayVote,
    /// Private wolf discussion. Answer with `speak`.
    WolfChat,
    /// Pick tonight's victim. Answer with `target`.
    WolfKill,
    /// Learn whether a player is a werewolf. Answer with `target`.
    SeerInspect,
    /// Save one player from tonight's kill. Answer with `target`.
    DoctorProtect,
}

/// Everything this player is allowed to know.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PlayerView {
    pub you: PlayerId,
    pub role: Role,
    pub phase: Phase,
    pub players: Vec<PublicPlayer>,
    /// Fellow werewolves; empty unless you are one.
    pub wolf_team: Vec<PlayerId>,
    pub public_log: Vec<PublicEvent>,
    pub private_log: Vec<PrivateEvent>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PublicPlayer {
    pub id: PlayerId,
    pub name: String,
    pub model: Option<String>,
    pub alive: bool,
    /// Revealed on death.
    pub revealed_role: Option<Role>,
    #[serde(default)]
    pub human: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PublicEvent {
    PhaseStarted {
        phase: Phase,
    },
    Spoke {
        player: PlayerId,
        text: String,
    },
    Voted {
        voter: PlayerId,
        target: Option<PlayerId>,
    },
    Died {
        player: PlayerId,
        role: Role,
        cause: DeathCause,
    },
    PeacefulNight,
    NoElimination,
    GameOver {
        winner: Team,
    },
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum DeathCause {
    Vote,
    Night,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PrivateEvent {
    WolfSpoke {
        player: PlayerId,
        text: String,
    },
    WolfVoted {
        voter: PlayerId,
        target: Option<PlayerId>,
    },
    Inspected {
        target: PlayerId,
        is_werewolf: bool,
    },
    Protected {
        target: PlayerId,
    },
}

/// `POST /v1/act`
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ActRequest {
    pub turn_id: u64,
    pub action: Action,
    /// Private reasoning shown to spectators only, never to other players.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    Speak { text: String },
    Target { target: Option<PlayerId> },
}

/// Body of every 4xx/5xx response.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ApiError {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_is_stable() {
        let act = ActRequest {
            turn_id: 7,
            action: Action::Target { target: Some(3) },
            reasoning: None,
        };
        assert_eq!(
            serde_json::to_string(&act).unwrap(),
            r#"{"turn_id":7,"action":{"type":"target","target":3}}"#
        );
        assert_eq!(
            serde_json::to_string(&Phase::Night { n: 2 }).unwrap(),
            r#"{"kind":"night","n":2}"#
        );
        assert_eq!(
            serde_json::to_string(&Poll::Wait).unwrap(),
            r#"{"status":"wait"}"#
        );
    }
}

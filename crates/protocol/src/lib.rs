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
    /// Village gunslinger with one bullet; shooting a non-wolf kills the sheriff too.
    Sheriff,
    /// Village matchmaker who links two Lovers on night 1.
    Cupid,
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
    /// A werewolf and a non-werewolf in love, once they are the last two alive. Only a winner.
    Lovers,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Phase {
    Lobby,
    Night { n: u32 },
    Day { n: u32 },
    Over,
}

/// `GET /v1/info`
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Info {
    pub version: u32,
    pub play_url: String,
    pub settings: SettingsBody,
    /// Escrow terms; present only for staked games.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stake: Option<StakeInfo>,
}

/// Lobby settings in whole seconds, as shown by `/v1/info` and changed through `/admin/settings`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SettingsBody {
    pub wolves: Option<u32>,
    pub sheriff: bool,
    pub cupid: bool,
    pub discussion_secs: u32,
    pub ai_chat_cooldown_secs: u32,
    pub ai_max_chat_messages: u32,
    pub human_turn_timeout_secs: u32,
}

/// A staked game's escrow terms. Hex values are 0x-prefixed; see `docs/SETTLEMENT.md`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct StakeInfo {
    pub chain_id: u64,
    /// Escrow contract address.
    pub escrow: String,
    /// Decimal.
    pub game_id: String,
    /// ERC-20 token address.
    pub token: String,
    /// Buy-in in the token's base units, decimal.
    pub stake: String,
    /// Address that signs receipts.
    pub operator: String,
    /// Operator's X25519 public key (32 bytes) for forced moves.
    pub enc_key: String,
    /// RPC endpoint the server suggests; players may use their own.
    #[serde(default)]
    pub rpc_url: Option<String>,
    pub min_seats: u8,
    pub max_seats: u8,
    /// Unix seconds.
    pub join_by: u64,
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
    /// Proof of a staked seat; required in staked games.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stake: Option<StakeJoin>,
}

/// Ties a join to its on-chain seat. Hex strings, 0x-prefixed.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct StakeJoin {
    /// Wallet that called `join` on the escrow.
    pub address: String,
    /// Ed25519 public key the seat registered.
    pub session_key: String,
    /// Session-key signature over the join message.
    pub signature: String,
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
    /// Counts chat windows and target rounds together from 0; signed in staked games.
    #[serde(default)]
    pub step: u32,
    /// Counts target rounds from 0; absent for chat turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
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
    /// Public day discussion, re-issued while the window is open. Answer with `speak`; empty
    /// text passes.
    DaySpeak,
    /// Vote to eliminate. Answer with `target` (null abstains).
    DayVote,
    /// Private wolf discussion, re-issued while the window is open. Answer with `speak`; empty
    /// text passes.
    WolfChat,
    /// Pick tonight's victim. Answer with `target`.
    WolfKill,
    /// Learn whether a player is a werewolf. Answer with `target`.
    SeerInspect,
    /// Save one player from tonight's kill. Answer with `target`.
    DoctorProtect,
    /// Fire the sheriff's one bullet before the vote. Answer with `target`; null holds fire.
    SheriffShoot,
    /// Cupid's first or second Lover, on night 1. Answer with `target`; null means no Lovers.
    CupidLink,
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
    /// Revealed on death, or when the sheriff fires.
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
    /// The sheriff fired; `Died` events follow.
    Shot {
        sheriff: PlayerId,
        target: PlayerId,
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
    /// Shot by the sheriff.
    Shot,
    /// The sheriff, after shooting a non-wolf.
    Backfire,
    /// A Lover, right after their partner died.
    Heartbreak,
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
    /// Cupid's two Lovers, in pick order.
    Linked {
        a: PlayerId,
        b: PlayerId,
    },
    /// Sent to each Lover about the other.
    InLove {
        partner: PlayerId,
        partner_role: Role,
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
    /// Public log hash the signature covers; staked games only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_hash: Option<String>,
    /// Session-key signature over the act message; staked games only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// `POST /v1/act` response.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ActResponse {
    /// The operator's promise to include the action; staked games only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<Receipt>,
}

/// Operator-signed proof that an action entered the transcript. Hex strings, 0x-prefixed.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub turn_id: u64,
    pub seat: u32,
    pub act_hash: String,
    /// `receiptDigest`, the Merkle leaf.
    pub digest: String,
    /// 65-byte secp256k1 signature over `digest`.
    pub signature: String,
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
            log_hash: None,
            signature: None,
        };
        assert_eq!(
            serde_json::to_string(&act).unwrap(),
            r#"{"turn_id":7,"action":{"type":"target","target":3}}"#
        );
        let signed = ActRequest {
            log_hash: Some("0x01".into()),
            signature: Some("0x02".into()),
            ..act
        };
        assert_eq!(
            serde_json::to_string(&signed).unwrap(),
            r#"{"turn_id":7,"action":{"type":"target","target":3},"log_hash":"0x01","signature":"0x02"}"#
        );
        assert_eq!(
            serde_json::to_string(&ActResponse::default()).unwrap(),
            "{}"
        );
        let join: JoinRequest = serde_json::from_str(r#"{"name":"a","model":null}"#).unwrap();
        assert!(join.stake.is_none());
        assert_eq!(
            serde_json::to_string(&join).unwrap(),
            r#"{"name":"a","model":null,"human":false}"#
        );
        assert_eq!(
            serde_json::to_string(&Phase::Night { n: 2 }).unwrap(),
            r#"{"kind":"night","n":2}"#
        );
        assert_eq!(
            serde_json::to_string(&Poll::Wait).unwrap(),
            r#"{"status":"wait"}"#
        );
        assert_eq!(
            serde_json::to_string(&PrivateEvent::InLove {
                partner: 2,
                partner_role: Role::Cupid,
            })
            .unwrap(),
            r#"{"type":"in_love","partner":2,"partner_role":"cupid"}"#
        );
    }
}

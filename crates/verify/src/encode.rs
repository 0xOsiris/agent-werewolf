//! Every hashed or signed byte string in `docs/SETTLEMENT.md`.

use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_sol_types::{SolType, sol};
use sha2::{Digest, Sha256};
use werewolf_engine::{Game, GameEvent};
use werewolf_protocol::{Action, PublicEvent, Team};

use crate::transcript::{OpEntry, Rules, Seat};

sol!(
    #[sol(all_derives)]
    "../../contracts/src/IWerewolfEscrow.sol"
);

pub use IWerewolfEscrow::PublicValues;

const ACT_TAG: &[u8] = b"werewolf/act/v1";
const JOIN_TAG: &[u8] = b"werewolf/join/v1";
const LOG_TAG: &[u8] = b"werewolf/log/v1";
const DEAL_TAG: &[u8] = b"werewolf/deal/v1";
const RECEIPT_TAG: &[u8] = b"werewolf/receipt/v1";

const OP_FORCE: u8 = 1;
const OP_CHECKPOINT: u8 = 2;

/// `abi.encode` argument lists, spelled as in the spec.
type ReceiptFields = sol!((bytes32, uint64, address, uint256, uint64, uint32, bytes32));
type OpFields = sol!((bytes32, uint8, uint32, uint32, bytes32));
type RulesFields = sol!((uint8, bool, bool));
type RosterFields = sol!((address[], bytes32[]));

/// The game's domain `D`, which binds every signature and hash to one escrow game.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Domain {
    pub chain_id: u64,
    pub escrow: Address,
    pub game_id: U256,
}

impl Domain {
    /// `be64(chainId) ‖ addr(escrow) ‖ u256(gameId)`.
    pub fn to_bytes(&self) -> [u8; 60] {
        let mut bytes = [0; 60];
        bytes[..8].copy_from_slice(&self.chain_id.to_be_bytes());
        bytes[8..28].copy_from_slice(self.escrow.as_slice());
        bytes[28..].copy_from_slice(&self.game_id.to_be_bytes::<32>());
        bytes
    }
}

/// The fields a session key signs for one action.
#[derive(Clone, Copy, Debug)]
pub struct Act<'a> {
    pub turn_id: u64,
    pub step: u32,
    pub seat: u32,
    /// A public log prefix hash the player saw.
    pub log_hash: B256,
    pub action: &'a Action,
}

impl Act<'_> {
    /// `M`, the bytes the session key signs.
    pub fn message(&self, domain: &Domain) -> Result<Vec<u8>, serde_json::Error> {
        let mut message = [
            ACT_TAG,
            &domain.to_bytes(),
            &self.turn_id.to_be_bytes(),
            &self.step.to_be_bytes(),
            &self.seat.to_be_bytes(),
            self.log_hash.as_slice(),
        ]
        .concat();
        serde_json::to_writer(&mut message, self.action)?;
        Ok(message)
    }

    /// The Merkle leaf for this act.
    pub fn receipt_digest(&self, domain: &Domain) -> Result<B256, serde_json::Error> {
        let act_hash = act_hash(&self.message(domain)?);
        Ok(receipt_digest(domain, self.turn_id, self.seat, act_hash))
    }
}

/// `actHash = sha256(M)`.
pub fn act_hash(message: &[u8]) -> B256 {
    sha256(&[message])
}

/// The bytes a session key signs to prove it holds `player`'s seat.
pub fn join_message(domain: &Domain, player: &Address) -> Vec<u8> {
    [JOIN_TAG, &domain.to_bytes(), player.as_slice()].concat()
}

/// `receiptDigest`, which the operator signs raw and commits to as a Merkle leaf.
pub fn receipt_digest(domain: &Domain, turn_id: u64, seat: u32, act_hash: B256) -> B256 {
    keccak256(ReceiptFields::abi_encode_params(&(
        keccak256(RECEIPT_TAG),
        domain.chain_id,
        domain.escrow,
        domain.game_id,
        turn_id,
        seat,
        act_hash,
    )))
}

/// Every prefix hash of the public log, `h₀` first.
#[derive(Clone, Debug)]
pub struct LogChain {
    hashes: Vec<B256>,
}

impl LogChain {
    pub fn new(domain: &Domain) -> Self {
        Self {
            hashes: vec![sha256(&[LOG_TAG, &domain.to_bytes()])],
        }
    }

    pub fn push(&mut self, event: &PublicEvent) -> Result<(), serde_json::Error> {
        let json = serde_json::to_vec(event)?;
        let next = sha256(&[self.head().as_slice(), &json]);
        self.hashes.push(next);
        Ok(())
    }

    /// Pushes the public events among `events`, in order.
    pub fn extend(&mut self, events: &[GameEvent]) -> Result<(), serde_json::Error> {
        for event in events {
            if let GameEvent::Public(event) = event {
                self.push(event)?;
            }
        }
        Ok(())
    }

    /// `hₙ` over the whole log so far.
    pub fn head(&self) -> B256 {
        // `new` seeds `h₀`, so the chain is never empty.
        self.hashes.last().copied().unwrap_or_default()
    }

    pub fn hashes(&self) -> &[B256] {
        &self.hashes
    }

    /// Whether `hash` is some `hₖ` so far.
    pub fn contains(&self, hash: &B256) -> bool {
        self.hashes.contains(hash)
    }
}

/// OpenZeppelin-compatible root: sorted pairs, the odd node carried up, `bytes32(0)` if empty.
pub fn merkle_root(leaves: &[B256]) -> B256 {
    let mut level = leaves.to_vec();
    while level.len() > 1 {
        level = next_level(&level);
    }
    level.first().copied().unwrap_or_default()
}

/// Sibling hashes from leaf `index` up, for `MerkleProof.verify`; `None` if out of range.
pub fn merkle_proof(leaves: &[B256], mut index: usize) -> Option<Vec<B256>> {
    if index >= leaves.len() {
        return None;
    }
    let mut proof = Vec::new();
    let mut level = leaves.to_vec();
    while level.len() > 1 {
        // The odd last node has no sibling; it moves up unchanged.
        if let Some(sibling) = level.get(index ^ 1) {
            proof.push(*sibling);
        }
        level = next_level(&level);
        index /= 2;
    }
    Some(proof)
}

fn next_level(level: &[B256]) -> Vec<B256> {
    level
        .chunks(2)
        .filter_map(|pair| pair.iter().copied().reduce(hash_pair))
        .collect()
}

fn hash_pair(a: B256, b: B256) -> B256 {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    keccak256([lo.as_slice(), hi.as_slice()].concat())
}

/// The contract's `opLogHash` after `entry`.
pub fn op_log_next(prev: B256, entry: &OpEntry) -> B256 {
    let (kind, seat, round, payload_hash) = match entry {
        OpEntry::Force {
            seat,
            round,
            payload,
        } => (OP_FORCE, *seat, *round, keccak256(payload)),
        OpEntry::Checkpoint { round } => (OP_CHECKPOINT, 0, *round, B256::ZERO),
    };
    keccak256(OpFields::abi_encode_params(&(
        prev,
        kind,
        seat,
        round,
        payload_hash,
    )))
}

/// `opLogHash` over a whole op log, starting from `bytes32(0)`.
pub fn op_log_hash(entries: &[OpEntry]) -> B256 {
    entries.iter().fold(B256::ZERO, op_log_next)
}

pub fn rules_hash(rules: &Rules) -> B256 {
    keccak256(RulesFields::abi_encode_params(&(
        rules.wolves,
        rules.sheriff,
        rules.cupid,
    )))
}

/// `keccak(abi.encode(address[] players, bytes32[] sessionKeys))`.
pub fn roster_hash(seats: &[Seat]) -> B256 {
    let players: Vec<Address> = seats.iter().map(|s| s.address).collect();
    let keys: Vec<B256> = seats.iter().map(|s| s.session_key).collect();
    keccak256(RosterFields::abi_encode_params(&(players, keys)))
}

/// The engine's ChaCha20 seed.
pub fn deal_seed(seed: &B256, randomness: &B256, roster_hash: &B256) -> [u8; 32] {
    keccak256(
        [
            DEAL_TAG,
            seed.as_slice(),
            randomness.as_slice(),
            roster_hash.as_slice(),
        ]
        .concat(),
    )
    .0
}

/// The contract's team number.
pub fn team_id(team: Team) -> u8 {
    match team {
        Team::Village => 0,
        Team::Wolves => 1,
        Team::Lovers => 2,
    }
}

/// Bitmap of [`Game::won`] over the first `seats` seats.
pub fn winners(game: &Game, seats: usize) -> U256 {
    (0..seats)
        .zip(0..)
        .filter(|&(_, seat)| game.won(seat))
        .fold(U256::ZERO, |bits, (bit, _)| bits | (U256::from(1) << bit))
}

/// `PublicValues` with 0x-hex fields and a decimal `gameId`, as `werewolf-verify` prints it.
pub fn public_values_json(values: &PublicValues) -> serde_json::Value {
    serde_json::json!({
        "chain_id": values.chainId,
        "escrow": values.escrow,
        "game_id": values.gameId.to_string(),
        "rules_hash": values.rulesHash,
        "roster_hash": values.rosterHash,
        "seed_commit": values.seedCommit,
        "randomness": values.randomness,
        "enc_key": values.encKey,
        "op_log_hash": values.opLogHash,
        "winner_team": values.winnerTeam,
        "winners": values.winners,
        "transcript_root": values.transcriptRoot,
    })
}

fn sha256(parts: &[&[u8]]) -> B256 {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    B256::from(<[u8; 32]>::from(hasher.finalize()))
}

//! Keeps `vectors.json` and `transcript.json` in sync with the code; other implementations test
//! against them. Regenerate with `UPDATE_VECTORS=1 cargo test -p werewolf-verify --test vectors`.

mod common;

use std::{fs, path::PathBuf};

use alloy_sol_types::SolValue;
use serde::Serialize;
use werewolf_protocol::{Action, DeathCause, Phase, PublicEvent, Role};
use werewolf_verify::{
    Decrypt,
    alloy_primitives::{Address, B256, B512, Bytes, keccak256},
    encode::{self, Act, Domain, LogChain},
    force,
    keys::{self, SigningKey},
    transcript::{OpEntry, Rules, Seat},
    verify,
};

#[derive(Serialize)]
struct Vectors {
    about: &'static str,
    domain: DomainVector,
    acts: Vec<ActVector>,
    join: JoinVector,
    log: LogVector,
    merkle: Vec<MerkleVector>,
    op_log: OpLogVector,
    rules: RulesVector,
    roster: RosterVector,
    deal: DealVector,
    force: ForceVector,
    transcript: TranscriptVector,
}

#[derive(Serialize)]
struct DomainVector {
    chain_id: u64,
    escrow: Address,
    game_id: String,
    /// `D`.
    bytes: Bytes,
}

#[derive(Serialize)]
struct ActVector {
    name: &'static str,
    secret_key: B256,
    session_key: B256,
    turn_id: u64,
    step: u32,
    seat: u32,
    log_hash: B256,
    action: Action,
    action_json: String,
    /// `M`.
    message: Bytes,
    act_hash: B256,
    signature: B512,
    receipt_digest: B256,
}

#[derive(Serialize)]
struct JoinVector {
    secret_key: B256,
    session_key: B256,
    player: Address,
    message: Bytes,
    signature: B512,
}

#[derive(Serialize)]
struct LogVector {
    events: Vec<PublicEvent>,
    event_json: Vec<String>,
    /// `h₀` to `hₙ`.
    hashes: Vec<B256>,
}

#[derive(Serialize)]
struct MerkleVector {
    leaves: Vec<B256>,
    root: B256,
    /// One proof per leaf, for OpenZeppelin's `MerkleProof.verify`.
    proofs: Vec<Vec<B256>>,
}

#[derive(Serialize)]
struct OpLogVector {
    entries: Vec<OpEntry>,
    /// `opLogHash` after each entry.
    hashes: Vec<B256>,
}

#[derive(Serialize)]
struct RulesVector {
    rules: Rules,
    rules_hash: B256,
}

#[derive(Serialize)]
struct RosterVector {
    seats: Vec<Seat>,
    roster_hash: B256,
}

#[derive(Serialize)]
struct DealVector {
    seed: B256,
    randomness: B256,
    roster_hash: B256,
    seed_commit: B256,
    /// Fed to `ChaCha20Rng::from_seed`.
    engine_seed: B256,
}

#[derive(Serialize)]
struct ForceVector {
    enc_secret: B256,
    enc_key: B256,
    ephemeral_secret: B256,
    ephemeral_public: B256,
    round: u32,
    seat: u32,
    action: Action,
    plaintext: String,
    aad: Bytes,
    payload: Bytes,
}

#[derive(Serialize)]
struct TranscriptVector {
    file: &'static str,
    enc_secret: B256,
    /// `abi.encode(PublicValues)` that `prove` receives.
    public_values: Bytes,
}

fn act_vector(
    domain: &Domain,
    name: &'static str,
    seat: u32,
    step: u32,
    log_hash: B256,
    action: Action,
) -> ActVector {
    let key = SigningKey::from_bytes(&[seat as u8 + 1; 32]);
    let turn_id = 7;
    let act = Act {
        turn_id,
        step,
        seat,
        log_hash,
        action: &action,
    };
    let message = act.message(domain).unwrap();
    let receipt_digest = act.receipt_digest(domain).unwrap();
    let signature = keys::sign(&key, &message);
    keys::verify(&keys::session_key(&key), &message, &signature).unwrap();
    ActVector {
        name,
        secret_key: B256::from(key.to_bytes()),
        session_key: keys::session_key(&key),
        turn_id,
        step,
        seat,
        log_hash,
        action_json: serde_json::to_string(&action).unwrap(),
        action,
        act_hash: encode::act_hash(&message),
        message: Bytes::from(message),
        signature,
        receipt_digest,
    }
}

/// OpenZeppelin's `MerkleProof.processProof`.
fn process_proof(leaf: B256, proof: &[B256]) -> B256 {
    proof.iter().fold(leaf, |node, &sibling| {
        let (lo, hi) = if node <= sibling {
            (node, sibling)
        } else {
            (sibling, node)
        };
        keccak256([lo.as_slice(), hi.as_slice()].concat())
    })
}

fn vectors() -> (Vectors, String) {
    let domain = common::domain();

    let events = vec![
        PublicEvent::PhaseStarted {
            phase: Phase::Night { n: 1 },
        },
        PublicEvent::Spoke {
            player: 2,
            text: "Grüße aus dem Café 🐺\n\"wolf\" is seat 4".into(),
        },
        PublicEvent::Died {
            player: 3,
            role: Role::Seer,
            cause: DeathCause::Night,
        },
    ];
    let mut log = LogChain::new(&domain);
    for event in &events {
        log.push(event).unwrap();
    }
    let log = LogVector {
        event_json: events
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect(),
        events,
        hashes: log.hashes().to_vec(),
    };

    let acts = vec![
        act_vector(
            &domain,
            "speak",
            1,
            3,
            log.hashes[3],
            Action::Speak {
                text: "I think seat 4 is lying.\nVote 4.".into(),
            },
        ),
        act_vector(
            &domain,
            "target_null",
            2,
            4,
            log.hashes[1],
            Action::Target { target: None },
        ),
    ];

    let join_key = SigningKey::from_bytes(&[9; 32]);
    let player = Address::with_last_byte(0xa9);
    let join_message = encode::join_message(&domain, &player);
    let join = JoinVector {
        secret_key: B256::from(join_key.to_bytes()),
        session_key: keys::session_key(&join_key),
        player,
        signature: keys::sign(&join_key, &join_message),
        message: Bytes::from(join_message),
    };

    let merkle = [1u32, 2, 3, 5]
        .into_iter()
        .map(|n| {
            let leaves: Vec<B256> = (0..n).map(|i| keccak256(i.to_be_bytes())).collect();
            let root = encode::merkle_root(&leaves);
            let proofs: Vec<Vec<B256>> = (0..leaves.len())
                .map(|i| encode::merkle_proof(&leaves, i).unwrap())
                .collect();
            for (leaf, proof) in leaves.iter().zip(&proofs) {
                assert_eq!(process_proof(*leaf, proof), root);
            }
            MerkleVector {
                leaves,
                root,
                proofs,
            }
        })
        .collect();
    assert_eq!(encode::merkle_root(&[]), B256::ZERO);

    let enc_secret = B256::repeat_byte(0x33);
    let enc_key = force::public_key(&enc_secret.0);
    let ephemeral = B256::repeat_byte(0x44);
    let (round, seat) = (2, 3);
    let action = Action::Target { target: Some(1) };
    let payload = force::encrypt(&enc_key, ephemeral.0, &domain, round, seat, &action).unwrap();
    assert_eq!(
        force::decrypt(&enc_secret.0, &domain, round, seat, &payload).unwrap(),
        action
    );
    let force = ForceVector {
        enc_secret,
        enc_key,
        ephemeral_secret: ephemeral,
        ephemeral_public: force::public_key(&ephemeral.0),
        round,
        seat,
        plaintext: serde_json::to_string(&action).unwrap(),
        action,
        aad: Bytes::from(force::aad(&domain, round, seat)),
        payload: Bytes::from(payload.clone()),
    };

    let entries = vec![
        OpEntry::Force {
            seat,
            round: 0,
            payload: Bytes::from(payload),
        },
        OpEntry::Checkpoint { round: 0 },
    ];
    let hashes = entries
        .iter()
        .scan(B256::ZERO, |hash, entry| {
            *hash = encode::op_log_next(*hash, entry);
            Some(*hash)
        })
        .collect();
    let op_log = OpLogVector { entries, hashes };

    let rules = Rules {
        wolves: 2,
        sheriff: true,
        cupid: true,
    };
    let seats: Vec<Seat> = common::header().seats.into_iter().take(3).collect();
    let roster_hash = encode::roster_hash(&seats);
    let seed = B256::repeat_byte(0x11);
    let randomness = B256::repeat_byte(0x22);

    let honest = common::honest_game().transcript;
    let values = verify(&honest, Decrypt::WithSecret(common::ENC_SECRET)).unwrap();
    let vectors = Vectors {
        about: "Fixed inputs and expected outputs for docs/SETTLEMENT.md. Hex is 0x-prefixed; \
                regenerate with UPDATE_VECTORS=1 cargo test -p werewolf-verify --test vectors.",
        domain: DomainVector {
            chain_id: domain.chain_id,
            escrow: domain.escrow,
            game_id: domain.game_id.to_string(),
            bytes: Bytes::copy_from_slice(&domain.to_bytes()),
        },
        acts,
        join,
        log,
        merkle,
        op_log,
        rules: RulesVector {
            rules,
            rules_hash: encode::rules_hash(&rules),
        },
        roster: RosterVector { seats, roster_hash },
        deal: DealVector {
            seed,
            randomness,
            roster_hash,
            seed_commit: keccak256(seed),
            engine_seed: B256::from(encode::deal_seed(&seed, &randomness, &roster_hash)),
        },
        force,
        transcript: TranscriptVector {
            file: "transcript.json",
            enc_secret: B256::from(common::ENC_SECRET),
            public_values: Bytes::from(values.abi_encode()),
        },
    };
    (vectors, pretty(&honest))
}

fn pretty(value: &impl Serialize) -> String {
    serde_json::to_string_pretty(value).unwrap() + "\n"
}

/// Writes `expected` when regenerating; otherwise fails if the file differs.
fn check_file(name: &str, expected: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(name);
    if std::env::var_os("UPDATE_VECTORS").is_some() {
        fs::write(&path, expected).unwrap();
    }
    let actual = fs::read_to_string(&path).unwrap_or_default();
    assert!(
        actual == expected,
        "tests/{name} is stale; regenerate with UPDATE_VECTORS=1"
    );
}

#[test]
fn vector_files_match_the_code() {
    let (vectors, transcript) = vectors();
    check_file("vectors.json", &pretty(&vectors));
    check_file("transcript.json", &transcript);
}

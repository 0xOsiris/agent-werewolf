//! A full staked game against a local anvil: signed acts, a forced move, a challenge proven with a
//! stand-in prover, a receipt dispute, settlement and payouts.

use std::{
    error::Error,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use alloy::{
    hex,
    network::TransactionBuilder,
    node_bindings::{Anvil, AnvilInstance},
    primitives::{Address, B256, Bytes, Signature, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol_types::SolValue,
};
use clap::Parser;
use futures_util::future::join_all;
use rand::{SeedableRng, rngs::StdRng, seq::IndexedRandom};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use werewolf_chain::{
    ChainClient, ChainConfig,
    bindings::{IERC20, IWerewolfEscrow::Status},
};
use werewolf_protocol::{
    ActRequest, ActResponse, Action, JoinRequest, JoinResponse, Poll, Receipt, StakeInfo,
    StakeJoin, Team, TurnKind,
};
use werewolf_server::Config;
use werewolf_verify::{
    Decrypt, Transcript,
    encode::{Act, Domain, LogChain, join_message},
    force,
    keys::{self, SigningKey},
    transcript::{AnswerSource, Step},
    verify,
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const STAKE: u64 = 100;
const SEATS: usize = 6;
const FUNDS: u64 = 10_000;
const WINDOW: u64 = 3_600;
const KEY: &str = "test-key";

fn artifact(name: &str) -> TestResult<Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../contracts/out/{name}.sol/{name}.json"));
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("{}: {err}; run `forge build` in contracts/", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

async fn deploy(provider: &impl Provider, name: &str, args: &[u8]) -> TestResult<Address> {
    let code = artifact(name)?["bytecode"]["object"]
        .as_str()
        .ok_or("artifact has no bytecode")?
        .to_owned();
    let mut code = hex::decode(code)?;
    code.extend_from_slice(args);
    let receipt = provider
        .send_transaction(TransactionRequest::default().with_deploy_code(code))
        .await?
        .get_receipt()
        .await?;
    Ok(receipt
        .contract_address
        .ok_or("deploy receipt has no contract address")?)
}

/// The deployed contracts and a client per funded account.
struct Chain {
    anvil: AnvilInstance,
    escrow: Address,
    token: Address,
    keys: Vec<PrivateKeySigner>,
}

impl Chain {
    async fn deploy() -> TestResult<Self> {
        let anvil = Anvil::new().try_spawn()?;
        let keys: Vec<PrivateKeySigner> = anvil
            .keys()
            .iter()
            .cloned()
            .map(PrivateKeySigner::from)
            .collect();
        let deployer = ProviderBuilder::new()
            .wallet(keys[0].clone())
            .connect_http(anvil.endpoint_url());
        let token = deploy(&deployer, "MockERC20", &[]).await?;
        let randomness = deploy(&deployer, "PrevrandaoRandomness", &[]).await?;
        let verifier = deploy(&deployer, "SP1MockVerifier", &[]).await?;
        let windows =
            (randomness, verifier, B256::ZERO, WINDOW, WINDOW, WINDOW).abi_encode_params();
        let escrow = deploy(&deployer, "WerewolfEscrow", &windows).await?;
        let erc20 = IERC20::new(token, &deployer);
        for key in &keys[1..] {
            erc20
                .mint(key.address(), U256::from(FUNDS))
                .send()
                .await?
                .get_receipt()
                .await?;
        }
        Ok(Self {
            anvil,
            escrow,
            token,
            keys,
        })
    }

    fn key_file(&self, dir: &Path, index: usize) -> TestResult<PathBuf> {
        let path = dir.join(format!("key{index}"));
        std::fs::write(&path, hex::encode(self.keys[index].to_bytes()))?;
        Ok(path)
    }

    async fn client(&self, dir: &Path, index: usize) -> TestResult<ChainClient> {
        let config = ChainConfig::new(
            self.anvil.endpoint_url(),
            self.anvil.chain_id(),
            self.escrow,
            Some(self.key_file(dir, index)?),
        );
        Ok(ChainClient::connect(config).await?)
    }

    async fn warp(&self, secs: u64) -> TestResult {
        let provider = ProviderBuilder::new().connect_http(self.anvil.endpoint_url());
        provider
            .raw_request::<_, Value>("evm_increaseTime".into(), (secs,))
            .await?;
        provider
            .raw_request::<_, Value>("evm_mine".into(), ())
            .await?;
        Ok(())
    }
}

/// Polls `check` every 200 ms until it yields a value, for at most `secs`.
async fn eventually<T, F, Fut>(secs: u64, what: &str, check: F) -> TestResult<T>
where
    F: Fn() -> Fut,
    Fut: Future<Output = TestResult<Option<T>>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(value) = check().await? {
            return Ok(value);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("timed out waiting for {what}").into());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

struct Server {
    base: String,
    http: Client,
    shutdown: CancellationToken,
    handle: JoinHandle<anyhow::Result<()>>,
}

impl Server {
    /// Starts a staked server for six seats on `chain`, operated by account 1.
    async fn start(chain: &Chain, dir: &Path, extra: &[String]) -> TestResult<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let args = [
            "werewolf-server".to_owned(),
            "--no-mdns".to_owned(),
            format!("--spectator-key={KEY}"),
            format!("--log-dir={}", dir.join("games").display()),
            format!("--public-url={base}"),
            format!("--stake={STAKE}"),
            format!("--chain-id={}", chain.anvil.chain_id()),
            format!("--rpc-url={}", chain.anvil.endpoint_url()),
            format!("--escrow={}", chain.escrow),
            format!("--token={}", chain.token),
            format!("--operator-key-file={}", chain.key_file(dir, 1)?.display()),
            format!("--min-players={SEATS}"),
            format!("--max-players={SEATS}"),
            "--wolves=1".to_owned(),
            "--turn-timeout-secs=3".to_owned(),
            "--settle-poll-secs=0.3".to_owned(),
            "--pace-ms=0".to_owned(),
            "--discussion-secs=1.5".to_owned(),
            "--wolf-chat-secs=1".to_owned(),
            "--chat-cooldown-secs=0.05".to_owned(),
            "--ai-chat-cooldown-secs=0.05".to_owned(),
            "--ai-chat-jitter-secs=0.05".to_owned(),
            "--quiet-secs=0.2".to_owned(),
        ];
        let config = Config::parse_from(args.iter().chain(extra));
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(werewolf_server::serve(config, listener, shutdown.clone()));
        Ok(Self {
            base,
            http: Client::new(),
            shutdown,
            handle,
        })
    }

    async fn stop(self) -> TestResult {
        self.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(10), self.handle).await???;
        Ok(())
    }

    /// The lobby's escrow terms once its game exists, and not game `not`.
    async fn stake_info(&self, not: Option<&str>) -> TestResult<StakeInfo> {
        eventually(30, "the escrow game", || async {
            let info: Value = self.get("/v1/info").await?.json().await?;
            let stake: Option<StakeInfo> = serde_json::from_value(info["stake"].clone()).ok();
            Ok(stake.filter(|s| Some(s.game_id.as_str()) != not))
        })
        .await
    }

    async fn get(&self, path: &str) -> TestResult<reqwest::Response> {
        Ok(self.http.get(format!("{}{path}", self.base)).send().await?)
    }

    async fn join(&self, req: &JoinRequest) -> TestResult<reqwest::Response> {
        Ok(self
            .http
            .post(format!("{}/v1/join", self.base))
            .json(req)
            .send()
            .await?)
    }
}

/// The seat that answers its first target turn only on-chain, and that turn's action.
#[derive(Default)]
struct Forcer {
    claimed: Option<(u32, u32, Action)>,
}

/// One seat's player: its keys, chain client and HTTP token.
struct Player {
    seat: u32,
    key: SigningKey,
    client: ChainClient,
    token: String,
}

struct Played {
    winner: Team,
    receipts: Vec<Receipt>,
}

/// Plays one seat to the end, signing every act and checking every receipt.
async fn play(
    server: &Server,
    player: &Player,
    domain: Domain,
    info: &StakeInfo,
    forcer: &Mutex<Forcer>,
) -> TestResult<Played> {
    let operator: Address = info.operator.parse()?;
    let enc_key: B256 = info.enc_key.parse()?;
    let mut rng = StdRng::seed_from_u64(player.seat.into());
    let mut receipts = Vec::new();
    let mut forced_turn = None;
    let mut spoke_in = None;
    loop {
        let res = server
            .http
            .get(format!("{}/v1/poll?wait=5", server.base))
            .bearer_auth(&player.token)
            .send()
            .await?;
        assert_eq!(res.status(), StatusCode::OK);
        let turn = match res.json::<Poll>().await? {
            Poll::Wait => continue,
            Poll::GameOver { winner, .. } => return Ok(Played { winner, receipts }),
            Poll::Turn(turn) if Some(turn.turn_id) == forced_turn => {
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
            Poll::Turn(turn) => turn,
        };
        let chat = matches!(turn.kind, TurnKind::DaySpeak | TurnKind::WolfChat);
        let action = if chat {
            // One message per window, then passes.
            let text = if spoke_in == Some(turn.view.phase) {
                String::new()
            } else {
                spoke_in = Some(turn.view.phase);
                format!("Seat {} here.\nTrust me.", player.seat)
            };
            Action::Speak { text }
        } else {
            Action::Target {
                target: turn.candidates.choose(&mut rng).copied(),
            }
        };

        if !chat {
            let round = turn.round.ok_or("target turns carry a round")?;
            let claimed = {
                let mut forcer = forcer.lock().map_err(|_| "forcer lock poisoned")?;
                let free = forcer.claimed.is_none();
                if free {
                    forcer.claimed = Some((player.seat, round, action.clone()));
                }
                free
            };
            if claimed {
                let payload = force::encrypt(
                    &enc_key,
                    [player.seat as u8 + 1; 32],
                    &domain,
                    round,
                    player.seat,
                    &action,
                )?;
                player
                    .client
                    .force_action(domain.game_id, round, payload.into())
                    .await?;
                forced_turn = Some(turn.turn_id);
                continue;
            }
        }

        let mut log = LogChain::new(&domain);
        for event in &turn.view.public_log {
            log.push(event)?;
        }
        let act = Act {
            turn_id: turn.turn_id,
            step: turn.step,
            seat: player.seat,
            log_hash: log.head(),
            action: &action,
        };
        let signature = keys::sign(&player.key, &act.message(&domain)?);
        let req = ActRequest {
            turn_id: turn.turn_id,
            action: action.clone(),
            reasoning: None,
            log_hash: Some(act.log_hash.to_string()),
            signature: Some(signature.to_string()),
        };
        let res = server
            .http
            .post(format!("{}/v1/act", server.base))
            .bearer_auth(&player.token)
            .json(&req)
            .send()
            .await?;
        match res.status() {
            StatusCode::OK => {}
            // A chat window can close between poll and act.
            StatusCode::CONFLICT if chat => continue,
            status => return Err(format!("act: {status}: {}", res.text().await?).into()),
        }
        let receipt = res.json::<ActResponse>().await?.receipt;
        let passes = matches!(&action, Action::Speak { text } if text.is_empty());
        match receipt {
            None if passes => {}
            None => return Err(format!("no receipt for {action:?}").into()),
            Some(_) if passes => return Err("a pass got a receipt".into()),
            Some(receipt) => {
                let digest: B256 = receipt.digest.parse()?;
                assert_eq!(digest, act.receipt_digest(&domain)?);
                assert_eq!((receipt.turn_id, receipt.seat), (turn.turn_id, player.seat));
                let signature = Signature::from_raw(&hex::decode(&receipt.signature)?)?;
                assert_eq!(signature.recover_address_from_prehash(&digest)?, operator);
                receipts.push(receipt);
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs anvil and forge build"]
async fn staked_game_settles_through_the_server() -> TestResult {
    // Server warnings show up next to a failure; a second init in this binary is harmless.
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let chain = Chain::deploy().await?;
    let dir = tempfile::tempdir()?;
    let log_dir = dir.path().join("games");
    // Stand-in prover: copies a proof fixture the test writes once the transcript is known.
    let fixture = dir.path().join("proof.json");
    let prover = dir.path().join("prover.sh");
    std::fs::write(
        &prover,
        format!(
            "#!/bin/sh\nwhile [ $# -gt 0 ]; do case \"$1\" in --out) out=\"$2\";; \
             --enc-secret-file) secret=\"$2\";; esac; shift; done\n\
             [ -s \"$secret\" ] || exit 3\ncp '{}' \"$out\"\n",
            fixture.display()
        ),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&prover, std::fs::Permissions::from_mode(0o755))?;
    }

    let extra = [
        format!("--prover-bin={}", prover.display()),
        format!("--auto-start={SEATS}"),
    ];
    let server = Server::start(&chain, dir.path(), &extra).await?;
    let info = server.stake_info(None).await?;
    assert_eq!(info.stake, STAKE.to_string());
    assert_eq!(info.min_seats as usize, SEATS);
    assert_eq!(
        info.rpc_url, None,
        "the operator's RPC URL is never published"
    );
    assert_eq!(server.get("/readyz").await?.status(), StatusCode::OK);
    let game_id: U256 = info.game_id.parse()?;
    let domain = Domain {
        chain_id: info.chain_id,
        escrow: chain.escrow,
        game_id,
    };
    let operator = chain.client(dir.path(), 1).await?;

    let plain = JoinRequest {
        name: "plain".to_owned(),
        model: None,
        human: false,
        stake: None,
    };
    let res = server.join(&plain).await?;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(res.text().await?.contains("needs a stake"));

    // Seats join the escrow in order, then over HTTP; the last join auto-starts the game.
    let mut players = Vec::new();
    for seat in 0..SEATS as u32 {
        let key = SigningKey::from_bytes(&[seat as u8 + 11; 32]);
        let client = chain.client(dir.path(), seat as usize + 2).await?;
        let (joined, _) = client.join(game_id, keys::session_key(&key)).await?;
        assert_eq!(joined.seat, seat);
        let address = client.address().ok_or("no signer")?;
        let join = |signature: String| JoinRequest {
            name: format!("bot{seat}"),
            model: Some("test/staked".to_owned()),
            human: false,
            stake: Some(StakeJoin {
                address: format!("{address:#x}"),
                session_key: keys::session_key(&key).to_string(),
                signature,
            }),
        };
        let proof = keys::sign(&key, &join_message(&domain, &address)).to_string();
        if seat == 0 {
            let forged = keys::sign(&key, b"something else").to_string();
            let res = server.join(&join(forged)).await?;
            assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
            // A rejoin gets a fresh token for the same seat.
            let first: JoinResponse = server.join(&join(proof.clone())).await?.json().await?;
            assert_eq!(first.player_id, 0);
        }
        let res = server.join(&join(proof)).await?;
        assert_eq!(res.status(), StatusCode::OK);
        let joined: JoinResponse = res.json().await?;
        assert_eq!(joined.player_id, seat);
        players.push(Player {
            seat,
            key,
            client,
            token: joined.token,
        });
    }

    let forcer = Mutex::new(Forcer::default());
    let games = players
        .iter()
        .map(|p| play(&server, p, domain, &info, &forcer));
    let results = tokio::time::timeout(Duration::from_secs(180), join_all(games))
        .await
        .map_err(|_| "game did not finish")?;
    let results: Vec<Played> = results.into_iter().collect::<TestResult<_>>()?;
    assert!(results.iter().all(|r| r.winner == results[0].winner));

    // The transcript replays to exactly what the server proposed.
    let proposed = eventually(30, "the proposal", || async {
        let game = operator.game_info(game_id).await?;
        Ok((game.status == Status::Proposed).then_some(game))
    })
    .await?;
    let transcript: Transcript = server.get("/v1/transcript").await?.json().await?;
    let by_id: Transcript = server
        .get(&format!("/v1/transcript?game_id={game_id}"))
        .await?
        .json()
        .await?;
    assert_eq!(transcript, by_id);
    let values = verify(&transcript, Decrypt::Trust)?;
    assert_eq!(values.winnerTeam, proposed.winnerTeam);
    assert_eq!(values.winners, proposed.winners);
    assert_eq!(values.transcriptRoot, proposed.transcriptRoot);
    assert_eq!(values.opLogHash, proposed.opLogHash);
    assert_eq!(values.rosterHash, proposed.rosterHash);
    assert_eq!(values.randomness, proposed.randomness);
    assert_eq!(values.seedCommit, proposed.seedCommit);
    assert_eq!(values.rulesHash, proposed.rulesHash);
    assert_eq!(values.encKey, proposed.encKey);
    let secret_file = log_dir.join("staked").join(format!(
        "{}-{:#x}-{game_id}.secret.json",
        info.chain_id, chain.escrow
    ));
    let secrets: Value = serde_json::from_slice(&std::fs::read(&secret_file)?)?;
    let enc_secret: B256 = secrets["enc_secret"]
        .as_str()
        .ok_or("no enc_secret")?
        .parse()?;
    assert_eq!(
        verify(&transcript, Decrypt::WithSecret(enc_secret.0))?,
        values
    );

    let (seat, round, action) = forcer
        .lock()
        .map_err(|_| "forcer lock poisoned")?
        .claimed
        .clone()
        .ok_or("nobody forced a move")?;
    let forced =
        transcript.steps.iter().any(|step| match step {
            Step::Round { round: r, answers } => *r == round && answers.iter().any(|a| {
                a.seat == seat
                    && matches!(&a.source, AnswerSource::Forced { action: f, .. } if *f == action)
            }),
            Step::Chat { .. } => false,
        });
    assert!(
        forced,
        "seat {seat}'s forced move for round {round} was applied"
    );

    // A challenge is proven by the stand-in prover, matching the proposal.
    let fixture_json = serde_json::json!({
        "public_values": Bytes::from(values.abi_encode()),
        "proof": "0x",
        "vkey": B256::ZERO,
        "cycles": 0,
    });
    std::fs::write(&fixture, serde_json::to_vec(&fixture_json)?)?;
    let challenger = chain.client(dir.path(), 8).await?;
    challenger.challenge(game_id).await?;
    eventually(30, "the proof", || async {
        Ok(operator.game_info(game_id).await?.proven.then_some(()))
    })
    .await?;

    // A disputed receipt is answered with its Merkle proof.
    let receipt = results
        .iter()
        .flat_map(|r| &r.receipts)
        .next()
        .ok_or("no receipts")?;
    let disputer = &players[receipt.seat as usize].client;
    disputer
        .dispute_receipt(
            game_id,
            receipt.turn_id,
            receipt.seat,
            receipt.act_hash.parse()?,
            hex::decode(&receipt.signature)?.into(),
        )
        .await?;
    eventually(30, "the dispute answer", || async {
        Ok((operator.game_info(game_id).await?.openDisputes == 0).then_some(()))
    })
    .await?;

    // Past the proof deadline the server finalizes and claims the operator's credit.
    chain.warp(WINDOW + 1).await?;
    let operator_address = operator.address().ok_or("no signer")?;
    eventually(30, "settlement", || async {
        let game = operator.game_info(game_id).await?;
        let owed = operator.claimable(game_id, operator_address).await?;
        Ok((game.status == Status::Settled && owed.is_zero()).then_some(()))
    })
    .await?;
    let bonds = STAKE * SEATS as u64 + STAKE + (STAKE / 4);
    assert_eq!(
        operator
            .token_balance(chain.token, operator_address)
            .await?,
        U256::from(FUNDS - STAKE * SEATS as u64 + bonds),
        "the bond, plus the beaten challenge and dispute bonds"
    );

    let winners = values.winners;
    let pot = U256::from(STAKE * SEATS as u64);
    let mut paid = U256::ZERO;
    for player in &players {
        let owed = operator
            .claimable(game_id, player.client.address().ok_or("no signer")?)
            .await?;
        assert_eq!(!owed.is_zero(), winners.bit(player.seat as usize));
        if !owed.is_zero() {
            let (claimed, _) = player.client.claim(game_id).await?;
            paid += claimed.amount;
        }
    }
    assert_eq!(paid, pot, "winners share the whole pot");
    server.stop().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs anvil and forge build"]
async fn unstarted_lobby_is_cancelled_and_replaced() -> TestResult {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let chain = Chain::deploy().await?;
    let dir = tempfile::tempdir()?;
    let server = Server::start(&chain, dir.path(), &[]).await?;
    let first = server.stake_info(None).await?;
    let operator = chain.client(dir.path(), 1).await?;
    let operator_address = operator.address().ok_or("no signer")?;
    let first_id: U256 = first.game_id.parse()?;
    let player = chain.client(dir.path(), 2).await?;
    player
        .join(
            first_id,
            keys::session_key(&SigningKey::from_bytes(&[1; 32])),
        )
        .await?;

    let res = server
        .http
        .post(format!("{}/admin/start", server.base))
        .header("x-spectator-key", KEY)
        .send()
        .await?;
    assert_eq!(res.status(), StatusCode::CONFLICT, "one seat is too few");
    assert!(res.text().await?.contains("seats on-chain"));

    // Once joins close, settlement cancels the game, claims the bond and opens a new lobby.
    chain.warp(15 * 60 + 1).await?;
    let second = server.stake_info(Some(&first.game_id)).await?;
    assert_ne!(second.game_id, first.game_id);
    eventually(30, "the cancelled game's bond", || async {
        let game = operator.game_info(first_id).await?;
        let owed = operator.claimable(first_id, operator_address).await?;
        Ok((game.status == Status::Cancelled && owed.is_zero()).then_some(()))
    })
    .await?;
    assert_eq!(
        operator
            .claimable(first_id, player.address().ok_or("no signer")?)
            .await?,
        U256::from(STAKE),
        "the seat can claim its stake back"
    );
    let snapshot = server
        .http
        .get(format!("{}/spectate/events?key={KEY}", server.base))
        .send()
        .await?
        .chunk()
        .await?
        .ok_or("no snapshot")?;
    let text = String::from_utf8_lossy(&snapshot);
    assert!(text.contains("closed before it started"), "{text}");
    server.stop().await
}

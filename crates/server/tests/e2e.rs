//! End-to-end tests over real HTTP against an in-process server.

use std::{collections::HashSet, path::Path, time::Duration};

use clap::Parser;
use futures_util::future::join_all;
use rand::{SeedableRng, rngs::StdRng, seq::IndexedRandom};
use reqwest::{Client, Response, StatusCode};
use serde_json::Value;
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use werewolf_protocol::{
    ActRequest, Action, JoinRequest, JoinResponse, Phase, PlayerView, Poll, PrivateEvent,
    PublicEvent, Team, Turn, TurnKind,
};
use werewolf_server::Config;

const KEY: &str = "test-key";
const SEED: u64 = 42;
/// Seat of the bot that never acts.
const SILENT: u32 = 5;
/// Markers that must only reach the right streams.
const REASONING: &str = "REASONING-MARKER";
const WOLF_CHAT: &str = "WOLF-CHAT-MARKER";
const DEAD_CHAT: &str = "DEAD-CHAT-MARKER";

struct Server {
    base: String,
    client: Client,
    shutdown: CancellationToken,
    handle: JoinHandle<anyhow::Result<()>>,
}

impl Server {
    async fn start(log_dir: &Path, args: &[&str]) -> Self {
        let key = format!("--spectator-key={KEY}");
        let seed = format!("--seed={SEED}");
        let log_dir = format!("--log-dir={}", log_dir.display());
        // Fast defaults; a test's own `args` replace any of these by flag name.
        let defaults = [
            "--pace-ms=0",
            "--discussion-secs=1.5",
            "--wolf-chat-secs=1",
            "--chat-cooldown-secs=0.05",
            "--ai-chat-cooldown-secs=0.05",
            "--ai-chat-jitter-secs=0.05",
            "--quiet-secs=0.2",
        ];
        let flag = |arg: &str| arg.split('=').next().unwrap_or_default().to_owned();
        let overridden: Vec<String> = args.iter().map(|a| flag(a)).collect();
        let base = ["werewolf-server", "--no-mdns", &key, &seed, &log_dir];
        let defaults = defaults
            .into_iter()
            .filter(|d| !overridden.contains(&flag(d)));
        let config =
            Config::parse_from(base.into_iter().chain(defaults).chain(args.iter().copied()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(werewolf_server::serve(config, listener, shutdown.clone()));
        Self {
            base,
            client: Client::new(),
            shutdown,
            handle,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn join(&self, name: &str) -> Response {
        self.join_as(name, false).await
    }

    async fn join_as(&self, name: &str, human: bool) -> Response {
        let req = JoinRequest {
            name: name.to_owned(),
            model: Some("test/random".to_owned()),
            human,
        };
        self.client
            .post(self.url("/v1/join"))
            .json(&req)
            .send()
            .await
            .unwrap()
    }

    async fn join_ok(&self, name: &str) -> JoinResponse {
        let res = self.join(name).await;
        assert_eq!(res.status(), StatusCode::OK);
        res.json().await.unwrap()
    }

    async fn get(&self, path: &str) -> Response {
        self.client.get(self.url(path)).send().await.unwrap()
    }

    async fn watch(&self, token: Option<&str>) -> Response {
        let path = match token {
            Some(token) => format!("/v1/watch?token={token}"),
            None => "/v1/watch".to_owned(),
        };
        self.get(&path).await
    }

    async fn settings_get(&self, key: Option<&str>) -> Response {
        let mut req = self.client.get(self.url("/admin/settings"));
        if let Some(key) = key {
            req = req.header("x-spectator-key", key);
        }
        req.send().await.unwrap()
    }

    async fn settings_put(&self, key: Option<&str>, body: &Value) -> Response {
        let mut req = self.client.put(self.url("/admin/settings")).json(body);
        if let Some(key) = key {
            req = req.header("x-spectator-key", key);
        }
        req.send().await.unwrap()
    }

    async fn admin(&self, path: &str, key: Option<&str>) -> Response {
        let mut req = self.client.post(self.url(path));
        if let Some(key) = key {
            req = req.header("x-spectator-key", key);
        }
        req.send().await.unwrap()
    }

    async fn poll(&self, token: &str, wait: u64) -> Response {
        let url = self.url(&format!("/v1/poll?wait={wait}"));
        self.client
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
    }

    async fn act(&self, token: &str, turn_id: u64, action: Action) -> Response {
        let req = ActRequest {
            turn_id,
            action,
            reasoning: Some(format!("{REASONING}: thinking out loud")),
        };
        self.client
            .post(self.url("/v1/act"))
            .bearer_auth(token)
            .json(&req)
            .send()
            .await
            .unwrap()
    }

    async fn stop(self) {
        self.shutdown.cancel();
        let served = tokio::time::timeout(Duration::from_secs(5), self.handle)
            .await
            .expect("graceful shutdown should not hang on open long-polls");
        served.unwrap().unwrap();
    }
}

async fn error_of(res: Response) -> String {
    let body: Value = res.json().await.unwrap();
    body["error"]
        .as_str()
        .expect("errors carry an ApiError body")
        .to_owned()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Style {
    /// Speaks twice per chat window, then passes; plays random legal moves.
    Chatty,
    /// Never acts.
    Silent,
    /// Plays like `Chatty` but never answers chat, then tries the closed window's turn.
    IgnoresChat,
}

fn is_chat(kind: TurnKind) -> bool {
    matches!(
        kind,
        TurnKind::DaySpeak | TurnKind::WolfChat | TurnKind::DeadChat
    )
}

/// Other players' roles show only after death, or for a sheriff who has fired.
fn assert_roles_hidden(view: &PlayerView) {
    let fired: Vec<u32> = view
        .public_log
        .iter()
        .filter_map(|e| match e {
            PublicEvent::Shot { sheriff, .. } => Some(*sheriff),
            _ => None,
        })
        .collect();
    for p in &view.players {
        if p.revealed_role.is_some() {
            assert!(
                !p.alive || fired.contains(&p.id),
                "role of living {} leaked to {}",
                p.id,
                view.you
            );
        }
    }
}

/// How one seat's game went.
struct Played {
    winner: Team,
    view: PlayerView,
    /// Every turn this seat answered, in order.
    answered: Vec<(Phase, TurnKind, u64)>,
}

/// Plays one seat until the game ends. Spares the silent seat so that it lives long enough to
/// time out.
async fn bot(server: &Server, token: String, seed: u64, style: Style) -> Played {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut answered = Vec::new();
    let mut chat_phase = None;
    let mut said = 0;
    let mut closed_chat_turn = None;
    loop {
        let res = server.poll(&token, 5).await;
        assert_eq!(res.status(), StatusCode::OK);
        match res.json::<Poll>().await.unwrap() {
            Poll::Wait => {}
            Poll::GameOver { winner, view } => {
                return Played {
                    winner,
                    view,
                    answered,
                };
            }
            Poll::Turn(turn)
                if style == Style::Silent
                    || (style == Style::IgnoresChat && is_chat(turn.kind)) =>
            {
                assert_ne!(
                    closed_chat_turn,
                    Some(turn.turn_id),
                    "a closed window's turn came back"
                );
                if is_chat(turn.kind) {
                    closed_chat_turn = Some(turn.turn_id);
                }
                // Past the deadline, which for chat is when the window closes.
                tokio::time::sleep(Duration::from_millis(turn.deadline_ms + 200)).await;
            }
            Poll::Turn(turn) => {
                assert_roles_hidden(&turn.view);
                // Agents skip ids they already answered, so a reused id would stall them.
                assert!(
                    answered.iter().all(|&(.., id)| id != turn.turn_id),
                    "turn {} came back after it was answered",
                    turn.turn_id
                );
                if let Some(stale) = closed_chat_turn.take() {
                    let late = Action::Speak {
                        text: "too late".to_owned(),
                    };
                    let res = server.act(&token, stale, late).await;
                    assert_eq!(res.status(), StatusCode::CONFLICT);
                    assert!(error_of(res).await.contains("stale turn"));
                }
                if chat_phase != Some(turn.view.phase) {
                    chat_phase = Some(turn.view.phase);
                    said = 0;
                }
                let spared: Vec<u32> = turn
                    .candidates
                    .iter()
                    .copied()
                    .filter(|&c| c != SILENT)
                    .collect();
                let action = match turn.kind {
                    // The armed sheriff fires on their first chance.
                    TurnKind::DaySpeak if !spared.is_empty() => Action::Target {
                        target: spared.choose(&mut rng).copied(),
                    },
                    TurnKind::DaySpeak | TurnKind::WolfChat | TurnKind::DeadChat if said >= 2 => {
                        Action::Speak {
                            text: String::new(),
                        }
                    }
                    TurnKind::DaySpeak => {
                        said += 1;
                        Action::Speak {
                            text: format!("I am player {}. Trust me.", turn.view.you),
                        }
                    }
                    TurnKind::WolfChat => {
                        said += 1;
                        Action::Speak {
                            text: format!("{WOLF_CHAT}: let's eat someone"),
                        }
                    }
                    TurnKind::DeadChat => {
                        said += 1;
                        Action::Speak {
                            text: format!("{DEAD_CHAT}: boo"),
                        }
                    }
                    _ => Action::Target {
                        target: spared.choose(&mut rng).copied(),
                    },
                };
                let res = server.act(&token, turn.turn_id, action).await;
                assert_eq!(res.status(), StatusCode::OK, "{}", error_of(res).await);
                answered.push((turn.view.phase, turn.kind, turn.turn_id));
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn six_bots_play_to_completion() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path(), &["--turn-timeout-secs=1"]).await;
    let mut tokens = Vec::new();
    for i in 0..6 {
        tokens.push(server.join_ok(&format!("bot{i}")).await.token);
    }
    assert_eq!(
        server.admin("/admin/start", Some(KEY)).await.status(),
        StatusCode::OK
    );

    let bots = tokens.into_iter().zip(0..).map(|(token, i)| {
        let style = if i == SILENT {
            Style::Silent
        } else {
            Style::Chatty
        };
        bot(&server, token, i.into(), style)
    });
    let results = tokio::time::timeout(Duration::from_secs(120), join_all(bots))
        .await
        .expect("game should finish");

    let winner = results[0].winner;
    assert!(
        results.iter().all(|r| r.winner == winner),
        "bots disagree on the winner"
    );
    assert!(results.iter().all(|r| matches!(r.view.phase, Phase::Over)));

    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(
        names.iter().any(|n| n.ends_with(&format!("-{SEED}.jsonl"))),
        "{names:?}"
    );
    let summary_name = names
        .iter()
        .find(|n| n.ends_with(&format!("-{SEED}-summary.json")))
        .unwrap_or_else(|| panic!("no summary in {names:?}"));
    let summary: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join(summary_name)).unwrap()).unwrap();
    assert_eq!(summary["seed"], SEED);
    assert_eq!(summary["winner"], serde_json::to_value(winner).unwrap());
    let players = summary["players"].as_array().unwrap();
    assert_eq!(players.len(), 6);
    assert!(players.iter().all(|p| p["human"] == false), "{summary}");
    for p in &players[..SILENT as usize] {
        assert_eq!(
            p["invalid_actions"], 0,
            "random bots only make legal moves: {p}"
        );
    }
    assert!(
        players[SILENT as usize]["timeouts"].as_u64().unwrap() > 0,
        "{summary}"
    );

    // The finished game counts at once, and a restart rebuilds the same board from the summary.
    let wins = players.iter().filter(|p| p["won"] == true).count();
    let expected = serde_json::json!({
        "min_games": 3,
        "rows": [{
            "name": "test/random",
            "human": false,
            "games": 6,
            "wins": wins,
            "wolf_games": players.iter().filter(|p| p["role"] == "werewolf").count(),
            "wolf_wins": players.iter().filter(|p| p["role"] == "werewolf" && p["won"] == true).count(),
        }],
    });
    let board: Value = server.get("/v1/leaderboard").await.json().await.unwrap();
    assert_eq!(board, expected);
    server.stop().await;
    let restarted = Server::start(dir.path(), &[]).await;
    let board: Value = restarted.get("/v1/leaderboard").await.json().await.unwrap();
    assert_eq!(board, expected, "rebuilt from the summary on disk");
    restarted.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_bad_auth_and_bad_requests() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path(), &["--turn-timeout-secs=30"]).await;

    let res = server
        .client
        .get(server.url("/spectate/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let res = server
        .client
        .get(server.url("/spectate/events?key=nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        server.admin("/admin/start", None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        server.poll("bogus", 0).await.status(),
        StatusCode::UNAUTHORIZED
    );

    let alice = server.join_ok("  alice\u{7} ").await;
    assert_eq!(server.join("ALICE").await.status(), StatusCode::CONFLICT);
    assert_eq!(
        server.join(" \n ").await.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );

    let res = server
        .act(&alice.token, 999, Action::Target { target: None })
        .await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert!(error_of(res).await.contains("stale"));

    let res = server.admin("/admin/start", Some(KEY)).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);

    let res = server.poll(&alice.token, 0).await;
    assert!(matches!(res.json::<Poll>().await.unwrap(), Poll::Wait));

    // The spectator stream opens with a snapshot of the lobby.
    let mut res = server
        .client
        .get(server.url(&format!("/spectate/events?key={KEY}")))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let chunk = res.chunk().await.unwrap().unwrap();
    let text = std::str::from_utf8(&chunk).unwrap();
    let data = text.lines().find_map(|l| l.strip_prefix("data:")).unwrap();
    let snapshot: Value = serde_json::from_str(data.trim()).unwrap();
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["players"][0]["name"], "alice");
    drop(res);

    let info: Value = server.get("/v1/info").await.json().await.unwrap();
    assert_eq!(info["version"], 1);
    assert_eq!(info["play_url"], format!("{}/play", server.base));
    let res = server.get("/qr.svg").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-type"], "image/svg+xml");
    assert!(res.text().await.unwrap().contains("<svg"));
    let res = server.get("/static/index.html").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        res.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    for path in [
        "/static/..%2fCargo.toml",
        "/static/.hidden",
        "/static/missing.js",
    ] {
        assert_eq!(
            server.get(path).await.status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }

    assert_eq!(
        server.watch(Some("bogus")).await.status(),
        StatusCode::UNAUTHORIZED
    );
    // A reset kills the token, so the player's stream ends right after the `reset` event.
    let res = server.watch(Some(&alice.token)).await;
    assert_eq!(res.status(), StatusCode::OK);
    let stream = tokio::spawn(read_events(res, |_| false));
    assert_eq!(
        server.admin("/admin/reset", Some(KEY)).await.status(),
        StatusCode::OK
    );
    let events = tokio::time::timeout(Duration::from_secs(5), stream)
        .await
        .expect("the stream should close after a reset")
        .unwrap();
    let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["snapshot", "reset"]);
    assert_eq!(
        server.watch(Some(&alice.token)).await.status(),
        StatusCode::UNAUTHORIZED
    );

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn validates_actions_against_the_pending_turn() {
    let dir = tempfile::tempdir().unwrap();
    let args = [
        "--turn-timeout-secs=1",
        "--human-turn-timeout-secs=30",
        "--public-url=http://example.test:9/",
        "--no-cupid",
    ];
    let server = Server::start(dir.path(), &args).await;
    let info: Value = server.get("/v1/info").await.json().await.unwrap();
    assert_eq!(info["play_url"], "http://example.test:9/play");
    let mut tokens = Vec::new();
    for i in 0..5 {
        let res = server.join_as(&format!("p{i}"), true).await;
        tokens.push(res.json::<JoinResponse>().await.unwrap().token);
    }
    assert_eq!(
        server.admin("/admin/start", Some(KEY)).await.status(),
        StatusCode::OK
    );
    assert_eq!(server.join("late").await.status(), StatusCode::CONFLICT);

    // Night 1 with one wolf: the wolf, seer, and doctor all get turns at once.
    let polls = join_all(tokens.iter().map(|t| server.poll(t, 3))).await;
    let mut turns: Vec<(String, Turn)> = Vec::new();
    for (token, res) in tokens.iter().zip(polls) {
        if let Poll::Turn(turn) = res.json::<Poll>().await.unwrap() {
            turns.push((token.clone(), turn));
        }
    }
    let (token, turn) = turns
        .iter()
        .find(|(_, t)| t.kind == TurnKind::SeerInspect)
        .expect("the seer should have a turn on night 1");
    assert!(turn.view.wolf_team.is_empty());
    assert!(turn.view.players.iter().all(|p| p.human));
    assert!(
        turn.deadline_ms > 5_000,
        "humans get the longer timeout: {}",
        turn.deadline_ms
    );

    let speak = Action::Speak {
        text: "hi".to_owned(),
    };
    let res = server.act(token, turn.turn_id, speak).await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let res = server
        .act(
            token,
            turn.turn_id,
            Action::Target {
                target: Some(turn.view.you),
            },
        )
        .await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let res = server
        .act(
            token,
            turn.turn_id,
            Action::Target {
                target: Some(turn.candidates[0]),
            },
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    let res = server
        .act(
            token,
            turn.turn_id,
            Action::Target {
                target: Some(turn.candidates[0]),
            },
        )
        .await;
    assert_eq!(res.status(), StatusCode::CONFLICT);

    // Resetting mid-game is refused.
    assert_eq!(
        server.admin("/admin/reset", Some(KEY)).await.status(),
        StatusCode::CONFLICT
    );
    server.stop().await;
}

/// Reads SSE `data:` payloads until `stop` matches one or the stream ends.
async fn read_events(mut res: Response, stop: impl Fn(&Value) -> bool) -> Vec<Value> {
    let mut buf = Vec::new();
    let mut events = Vec::new();
    while let Some(chunk) = res.chunk().await.unwrap() {
        buf.extend_from_slice(&chunk);
        while let Some(end) = buf.windows(2).position(|w| w == b"\n\n") {
            let frame: Vec<u8> = buf.drain(..end + 2).collect();
            let frame = std::str::from_utf8(&frame).unwrap();
            for data in frame.lines().filter_map(|l| l.strip_prefix("data:")) {
                let event: Value = serde_json::from_str(data.trim_start()).unwrap();
                let done = stop(&event);
                events.push(event);
                if done {
                    return events;
                }
            }
        }
    }
    events
}

fn is_game_over(event: &Value) -> bool {
    event["type"] == "public" && event["event"]["type"] == "game_over"
}

/// The full `roles` reveal that follows game over.
fn is_final_roles(event: &Value) -> bool {
    event["type"] == "roles" && event["roles"].as_array().unwrap().len() == 8
}

/// Whether `event` is `viewer`'s death, or a snapshot that includes it.
fn died(viewer: Option<usize>, event: &Value) -> bool {
    match event["type"].as_str() {
        Some("snapshot") => event["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| died(viewer, e)),
        Some("public") => {
            event["event"]["type"] == "died"
                && viewer.is_some_and(|me| event["event"]["player"] == me)
        }
        _ => false,
    }
}

/// Asserts `viewer` (a seat, or `None` for the audience) may see `event`; `over` reveals roles,
/// and `dead` (the viewer has died) reveals the dead chat.
fn assert_visible(viewer: Option<usize>, roles: &[String], over: bool, dead: bool, event: &Value) {
    let role = viewer.map(|me| roles[me].as_str());
    let wolf = role == Some("werewolf");
    let sees_turn = |turn: &Value| {
        let kind = turn["kind"].as_str().unwrap();
        let player = turn["player"].as_u64().unwrap() as usize;
        matches!(kind, "day_speak" | "day_vote")
            || Some(player) == viewer
            || (wolf && matches!(kind, "wolf_chat" | "wolf_kill"))
    };
    let sees_roles = |entries: &Value, over: bool| {
        let entries = entries.as_array().unwrap();
        if over {
            assert_eq!(entries.len(), roles.len(), "game over reveals every role");
        }
        for entry in entries {
            let player = entry["player"].as_u64().unwrap() as usize;
            let allowed = over || Some(player) == viewer || (wolf && entry["role"] == "werewolf");
            assert!(allowed, "{viewer:?} saw {entry}");
            assert_eq!(entry["role"], roles[player].as_str());
        }
    };
    match event["type"].as_str().unwrap() {
        "reasoning" => panic!("reasoning leaked to {viewer:?}"),
        "private" => {
            let by_role = match role {
                Some("werewolf") => "wolves",
                Some("seer") => "seer",
                Some("doctor") => "doctor",
                Some("cupid") => "cupid",
                _ => "nobody",
            };
            let audience = &event["audience"];
            let own = viewer.is_some_and(|me| audience["lover"] == me);
            let ghost = dead && audience == "dead";
            assert!(
                audience == by_role || own || ghost,
                "{viewer:?} saw {event}"
            );
        }
        "turn_started" | "turn_ended" => {
            assert!(
                !is_chat_kind(&event["kind"]),
                "chat turns have no turn events: {event}"
            );
            assert!(sees_turn(event), "{viewer:?} saw {event}");
        }
        "chat_window" => {
            let allowed = match event["kind"].as_str().unwrap() {
                "day_speak" => true,
                "wolf_chat" => wolf,
                "dead_chat" => dead,
                _ => false,
            };
            assert!(allowed, "{viewer:?} saw {event}");
        }
        "roles" => sees_roles(&event["roles"], over),
        "snapshot" => {
            let over = !event["winner"].is_null();
            sees_roles(&event["roles"], over);
            for pending in event["pending"].as_array().unwrap() {
                assert!(
                    !is_chat_kind(&pending["kind"]),
                    "chat in pending: {pending}"
                );
                assert!(sees_turn(pending), "{viewer:?} saw pending {pending}");
            }
            let fired: Vec<&Value> = event["events"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["event"]["type"] == "shot")
                .map(|e| &e["event"]["sheriff"])
                .collect();
            for p in event["players"].as_array().unwrap() {
                let revealed = !p["revealed_role"].is_null();
                let allowed = p["alive"] == false || fired.contains(&&p["id"]);
                assert!(!revealed || allowed, "{viewer:?} saw the role of {p}");
            }
            let chat = &event["chat"];
            assert!(
                chat.is_null() || chat["kind"] == "day_speak" || wolf,
                "{viewer:?} saw {chat}"
            );
            let mut dead = false;
            for inner in event["events"].as_array().unwrap() {
                dead |= died(viewer, inner);
                assert_visible(viewer, roles, over, dead, inner);
            }
            let dead_chat = &event["dead_chat"];
            assert!(dead_chat.is_null() || dead, "{viewer:?} saw {dead_chat}");
        }
        _ => {}
    }
}

fn is_chat_kind(kind: &Value) -> bool {
    kind == "day_speak" || kind == "wolf_chat" || kind == "dead_chat"
}

/// Checks a whole stream for leaks and, for wolves, that the pack's chat arrived.
fn assert_stream(viewer: Option<usize>, roles: &[String], events: &[Value]) {
    assert_no_leaks(viewer, roles, events);
    let wolf = viewer.is_some_and(|me| roles[me] == "werewolf");
    let text = serde_json::to_string(events).unwrap();
    assert!(
        !wolf || text.contains(WOLF_CHAT),
        "wolf chat missing for {viewer:?}"
    );
}

/// Checks a whole stream for anything `viewer` may not see.
fn assert_no_leaks(viewer: Option<usize>, roles: &[String], events: &[Value]) {
    assert_eq!(events[0]["type"], "snapshot");
    let (mut over, mut dead) = (false, false);
    for event in events {
        dead |= died(viewer, event);
        assert_visible(viewer, roles, over, dead, event);
        over |= is_game_over(event);
    }
    let text = serde_json::to_string(events).unwrap();
    assert!(!text.contains(REASONING), "reasoning leaked to {viewer:?}");
    let wolf = viewer.is_some_and(|me| roles[me] == "werewolf");
    assert!(
        wolf || !text.contains(WOLF_CHAT),
        "wolf chat leaked to {viewer:?}"
    );
    assert!(
        dead || !text.contains(DEAD_CHAT),
        "dead chat leaked to {viewer:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_streams_only_show_what_each_player_may_know() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path(), &["--turn-timeout-secs=2"]).await;
    let mut tokens = Vec::new();
    // Eight seats give two wolves, so night 1 has wolf chat.
    for i in 0..8 {
        tokens.push(server.join_ok(&format!("bot{i}")).await.token);
    }
    // Subscribe before the start so each stream covers the whole game.
    let mut streams = Vec::new();
    for token in &tokens {
        let res = server.watch(Some(token)).await;
        assert_eq!(res.status(), StatusCode::OK);
        streams.push(tokio::spawn(read_events(res, is_final_roles)));
    }
    let audience = tokio::spawn(read_events(server.watch(None).await, is_final_roles));
    assert_eq!(
        server.admin("/admin/start", Some(KEY)).await.status(),
        StatusCode::OK
    );
    let bots = tokens
        .iter()
        .cloned()
        .zip(0..)
        .map(|(token, i)| bot(&server, token, i, Style::Chatty));
    tokio::time::timeout(Duration::from_secs(120), join_all(bots))
        .await
        .expect("game should finish");

    let summary = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().ends_with("-summary.json"))
        .unwrap();
    let summary: Value = serde_json::from_slice(&std::fs::read(summary).unwrap()).unwrap();
    let players = summary["players"].as_array().unwrap();
    let roles: Vec<String> = players
        .iter()
        .map(|p| p["role"].as_str().unwrap().to_owned())
        .collect();
    // Bots always pick a target, so Cupid links two lovers.
    assert_eq!(roles.iter().filter(|r| *r == "cupid").count(), 1);
    assert_eq!(
        players.iter().filter(|p| !p["lover_of"].is_null()).count(),
        2,
        "{summary}"
    );
    assert!(players.iter().any(|p| p["won"] == true), "{summary}");

    for (me, stream) in streams.into_iter().enumerate() {
        let events = tokio::time::timeout(Duration::from_secs(10), stream)
            .await
            .unwrap()
            .unwrap();
        assert!(
            is_final_roles(events.last().unwrap()),
            "stream {me} ended early"
        );
        assert_stream(Some(me), &roles, &events);
        let wolf_window = events
            .iter()
            .any(|e| e["type"] == "chat_window" && e["kind"] == "wolf_chat");
        assert_eq!(
            wolf_window,
            roles[me] == "werewolf",
            "wolf chat window for {me}"
        );
        if roles[me] == "sheriff" {
            let died_night_one =
                summary["players"][me]["died_on"] == serde_json::json!({ "kind": "night", "n": 1 });
            let fired = events.iter().any(|e| {
                e["type"] == "public" && e["event"]["type"] == "shot" && e["event"]["sheriff"] == me
            });
            assert!(
                fired || died_night_one,
                "the sheriff fires during day 1's discussion"
            );
        }
        assert!(
            events.iter().all(|e| e["kind"] != "sheriff_shoot"),
            "the shot has no turn of its own"
        );
        let private = |kind: &str| -> Vec<&Value> {
            events
                .iter()
                .filter(|e| e["type"] == "private" && e["event"]["type"] == kind)
                .map(|e| &e["event"])
                .collect()
        };
        let link_turns = events
            .iter()
            .filter(|e| e["type"] == "turn_started" && e["kind"] == "cupid_link")
            .count();
        let cupid = roles[me] == "cupid";
        assert_eq!(link_turns, if cupid { 2 } else { 0 }, "link turns for {me}");
        assert_eq!(
            private("linked").len(),
            usize::from(cupid),
            "linked for {me}"
        );
        let partners: Vec<&Value> = private("in_love").iter().map(|e| &e["partner"]).collect();
        let lover_of = &summary["players"][me]["lover_of"];
        let expected: Vec<&Value> = if lover_of.is_null() {
            Vec::new()
        } else {
            vec![lover_of]
        };
        assert_eq!(partners, expected, "in_love for {me}");
        let start_roles = events.iter().find(|e| e["type"] == "roles").unwrap();
        let expected = if roles[me] == "werewolf" { 2 } else { 1 };
        assert_eq!(start_roles["roles"].as_array().unwrap().len(), expected);

        // A late subscriber's snapshot replays the whole game, filtered the same way.
        let late = read_events(server.watch(Some(&tokens[me])).await, |_| true).await;
        assert_stream(Some(me), &roles, &late);
    }
    let events = tokio::time::timeout(Duration::from_secs(10), audience)
        .await
        .unwrap()
        .unwrap();
    assert_stream(None, &roles, &events);
    let late = read_events(server.watch(None).await, |_| true).await;
    assert_stream(None, &roles, &late);

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wolf_count_needs_enough_players() {
    assert!(Config::try_parse_from(["werewolf-server", "--wolves=0"]).is_err());
    let too_many = Config::parse_from(["werewolf-server", "--wolves=6"]);
    assert!(too_many.validate().is_err(), "6 wolves need 13 seats");
    let seven = |extra: &[&str]| {
        let args = ["werewolf-server", "--wolves=3", "--max-players=7"];
        Config::parse_from(args.iter().chain(extra)).validate()
    };
    assert!(
        seven(&[]).is_err(),
        "3 wolves + sheriff + cupid need 8 seats"
    );
    assert!(seven(&["--no-cupid"]).is_ok());
    assert!(
        seven(&["--no-cupid", "--cupid"]).is_err(),
        "the last flag wins"
    );

    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path(), &["--wolves=3", "--auto-start=5"]).await;
    // Auto-start is refused at five and six players, so the lobby stays open.
    for i in 0..6 {
        server.join_ok(&format!("p{i}")).await;
    }
    let res = server.admin("/admin/start", Some(KEY)).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(
        error_of(res).await,
        "need at least 8 players for 3 wolves + sheriff + cupid"
    );
    server.join_ok("p6").await;
    server.join_ok("p7").await;
    assert_eq!(server.join("late").await.status(), StatusCode::CONFLICT);
    server.stop().await;
}

/// Reads summary JSON from the only game in `dir`.
fn read_summary(dir: &Path) -> Value {
    let path = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().ends_with("-summary.json"))
        .expect("summary written");
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_a_chat_window_cancels_turns_without_timeouts() {
    let dir = tempfile::tempdir().unwrap();
    let args = ["--turn-timeout-secs=5", "--discussion-secs=1", "--no-cupid"];
    let server = Server::start(dir.path(), &args).await;
    let mut tokens = Vec::new();
    for i in 0..5 {
        tokens.push(server.join_ok(&format!("bot{i}")).await.token);
    }
    assert_eq!(
        server.admin("/admin/start", Some(KEY)).await.status(),
        StatusCode::OK
    );
    // Nobody answers chat, so every window runs out; each bot then acts on its dead turn.
    let bots = tokens
        .into_iter()
        .zip(0..)
        .map(|(token, i)| bot(&server, token, i, Style::IgnoresChat));
    tokio::time::timeout(Duration::from_secs(120), join_all(bots))
        .await
        .expect("game should finish");

    let summary = read_summary(dir.path());
    for p in summary["players"].as_array().unwrap() {
        assert_eq!(p["timeouts"], 0, "a closing window is not a timeout: {p}");
        assert_eq!(
            p["invalid_actions"], 0,
            "stale chat answers are not invalid: {p}"
        );
    }
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_messages_skip_the_pace() {
    let dir = tempfile::tempdir().unwrap();
    let args = [
        "--pace-ms=1000",
        "--turn-timeout-secs=10",
        "--discussion-secs=3",
        "--no-cupid",
        // A sheriff bot fires at once, which could end the game before anyone speaks.
        "--no-sheriff",
    ];
    let server = Server::start(dir.path(), &args).await;
    let mut tokens = Vec::new();
    for i in 0..5 {
        tokens.push(server.join_ok(&format!("bot{i}")).await.token);
    }
    let res = server.get(&format!("/spectate/events?key={KEY}")).await;
    let window_closed =
        |e: &Value| e["type"] == "chat_window" && e["kind"] == "day_speak" && e["open"] == false;
    let feed = tokio::spawn(read_timed_events(res, window_closed));
    assert_eq!(
        server.admin("/admin/start", Some(KEY)).await.status(),
        StatusCode::OK
    );
    let bots: Vec<_> = tokens
        .into_iter()
        .zip(0..)
        .map(|(token, i)| bot(&server, token, i, Style::Chatty))
        .collect();
    let play = join_all(bots);
    let events = tokio::select! {
        events = feed => events.unwrap(),
        _ = play => panic!("the game ended before day 1's discussion closed"),
        () = tokio::time::sleep(Duration::from_secs(60)) => panic!("no discussion within 60s"),
    };

    let spoke: Vec<tokio::time::Instant> = events
        .iter()
        .filter(|(_, e)| e["type"] == "public" && e["event"]["type"] == "spoke")
        .map(|(at, _)| *at)
        .collect();
    assert!(spoke.len() >= 5, "every bot speaks: {}", spoke.len());
    // Paced at 1s each, five messages would take at least 4s.
    let spread = spoke[4] - spoke[0];
    assert!(
        spread < Duration::from_millis(800),
        "chat was paced: {spread:?}"
    );
    server.stop().await;
}

/// Like [`read_events`], stamping each event with its arrival time.
async fn read_timed_events(
    mut res: Response,
    stop: impl Fn(&Value) -> bool,
) -> Vec<(tokio::time::Instant, Value)> {
    let mut buf = Vec::new();
    let mut events = Vec::new();
    while let Some(chunk) = res.chunk().await.unwrap() {
        let at = tokio::time::Instant::now();
        buf.extend_from_slice(&chunk);
        while let Some(end) = buf.windows(2).position(|w| w == b"\n\n") {
            let frame: Vec<u8> = buf.drain(..end + 2).collect();
            let frame = std::str::from_utf8(&frame).unwrap();
            for data in frame.lines().filter_map(|l| l.strip_prefix("data:")) {
                let event: Value = serde_json::from_str(data.trim_start()).unwrap();
                let done = stop(&event);
                events.push((at, event));
                if done {
                    return events;
                }
            }
        }
    }
    events
}

#[tokio::test(flavor = "multi_thread")]
async fn reissued_chat_turns_get_fresh_ids() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path(), &["--turn-timeout-secs=5"]).await;
    let mut tokens = Vec::new();
    for i in 0..6 {
        tokens.push(server.join_ok(&format!("bot{i}")).await.token);
    }
    assert_eq!(
        server.admin("/admin/start", Some(KEY)).await.status(),
        StatusCode::OK
    );
    let bots = tokens
        .into_iter()
        .zip(0..)
        .map(|(token, i)| bot(&server, token, i, Style::Chatty));
    let results = tokio::time::timeout(Duration::from_secs(120), join_all(bots))
        .await
        .expect("game should finish");

    let ids: Vec<u64> = results
        .iter()
        .flat_map(|r| r.answered.iter().map(|&(.., id)| id))
        .collect();
    let unique: HashSet<u64> = ids.iter().copied().collect();
    assert_eq!(unique.len(), ids.len(), "a turn_id was reused");
    // Chatty bots speak twice and then pass, so day 1 re-issues at least two chat turns each.
    let day_one_chat = |r: &Played| {
        r.answered
            .iter()
            .filter(|&&(phase, kind, _)| phase == Phase::Day { n: 1 } && kind == TurnKind::DaySpeak)
            .count()
    };
    assert!(
        results.iter().any(|r| day_one_chat(r) >= 3),
        "expected re-issued chat turns on day 1"
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_are_lobby_only_validated_and_persistent() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path(), &["--turn-timeout-secs=5"]).await;
    let new = serde_json::json!({
        "wolves": 2,
        "sheriff": false,
        "cupid": false,
        "discussion_secs": 30,
        "ai_chat_cooldown_secs": 1,
        "ai_max_chat_messages": 2,
        "human_turn_timeout_secs": 10,
    });

    assert_eq!(
        server.settings_get(None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        server.settings_put(Some("nope"), &new).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let initial: Value = server.settings_get(Some(KEY)).await.json().await.unwrap();
    assert_eq!(initial["sheriff"], true);
    assert_eq!(initial["cupid"], true);
    assert_eq!(initial["wolves"], Value::Null);

    // Everyone may see settings: spectators, players, and the audience.
    let is_settings = |e: &Value| e["type"] == "settings";
    let spectator = server.get(&format!("/spectate/events?key={KEY}")).await;
    let spectator = tokio::spawn(read_events(spectator, is_settings));
    let audience = tokio::spawn(read_events(server.watch(None).await, is_settings));

    for (patch, field) in [
        (serde_json::json!({ "wolves": 4 }), "wolves"),
        (
            serde_json::json!({ "discussion_secs": 10 }),
            "discussion_secs",
        ),
        (
            serde_json::json!({ "ai_chat_cooldown_secs": 0 }),
            "ai_chat_cooldown_secs",
        ),
        (
            serde_json::json!({ "ai_max_chat_messages": 21 }),
            "ai_max_chat_messages",
        ),
        (
            serde_json::json!({ "human_turn_timeout_secs": 301 }),
            "human_turn_timeout_secs",
        ),
    ] {
        let mut body = new.clone();
        body.as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        let res = server.settings_put(Some(KEY), &body).await;
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY, "{field}");
        assert!(error_of(res).await.contains(field));
    }
    for field in ["sheriff", "cupid"] {
        let mut partial = new.clone();
        partial.as_object_mut().unwrap().remove(field);
        assert_eq!(
            server.settings_put(Some(KEY), &partial).await.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "PUT needs the full object, including {field}"
        );
    }

    let res = server.settings_put(Some(KEY), &new).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.json::<Value>().await.unwrap(), new);
    for stream in [spectator, audience] {
        let events = tokio::time::timeout(Duration::from_secs(5), stream)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(events[0]["settings"], initial, "snapshots carry settings");
        assert_eq!(events.last().unwrap()["settings"], new);
    }
    let info: Value = server.get("/v1/info").await.json().await.unwrap();
    assert_eq!(info["settings"], new);

    // Settings survive a reset and apply at the next start.
    assert_eq!(
        server.admin("/admin/reset", Some(KEY)).await.status(),
        StatusCode::OK
    );
    let kept: Value = server.settings_get(Some(KEY)).await.json().await.unwrap();
    assert_eq!(kept, new);
    for i in 0..5 {
        server.join_ok(&format!("p{i}")).await;
    }
    assert_eq!(
        server.admin("/admin/start", Some(KEY)).await.status(),
        StatusCode::OK
    );
    let res = server.get(&format!("/spectate/events?key={KEY}")).await;
    let snapshot = read_events(res, |_| true).await.remove(0);
    assert_eq!(snapshot["settings"], new);
    let roles: Vec<&str> = snapshot["roles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles.iter().filter(|&&r| r == "werewolf").count(), 2);
    assert!(!roles.contains(&"sheriff"));
    assert!(!roles.contains(&"cupid"));

    let res = server.settings_put(Some(KEY), &new).await;
    assert_eq!(res.status(), StatusCode::CONFLICT, "lobby only");
    server.stop().await;
}

/// Plays one seat of a scripted day 1: Cupid links `a` and `b`, the wolf kills `a`, so `b` dies
/// of heartbreak. Living seats return at the vote; ghosts once the dead chat closes. A `mute`
/// ghost holds its dead chat turn without answering.
async fn scripted(server: &Server, token: &str, (a, b): (u32, u32), mute: bool) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    let mut posted = false;
    loop {
        let res = server.poll(token, 2).await;
        assert_eq!(res.status(), StatusCode::OK);
        let turn = match res.json::<Poll>().await.unwrap() {
            Poll::Turn(turn) => turn,
            Poll::Wait if turns.iter().any(|t| t.kind == TurnKind::DeadChat) => return turns,
            Poll::Wait => continue,
            Poll::GameOver { .. } => panic!("the game ended before day 1's vote"),
        };
        let (id, kind) = (turn.turn_id, turn.kind);
        let action = match kind {
            TurnKind::DayVote => {
                turns.push(turn);
                return turns;
            }
            TurnKind::DeadChat if mute => {
                turns.push(turn);
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            TurnKind::DaySpeak | TurnKind::DeadChat => {
                let text = match (posted, kind) {
                    (true, _) => String::new(),
                    (false, TurnKind::DeadChat) => format!("{DEAD_CHAT}: boo"),
                    (false, _) => "Morning, village.".to_owned(),
                };
                posted = true;
                Action::Speak { text }
            }
            TurnKind::CupidLink => Action::Target {
                target: Some(if turn.candidates.contains(&a) { a } else { b }),
            },
            TurnKind::WolfKill => Action::Target { target: Some(a) },
            _ => Action::Target { target: None },
        };
        turns.push(turn);
        let res = server.act(token, id, action).await;
        // A chat answer can race the window's close.
        if !(is_chat(kind) && res.status() == StatusCode::CONFLICT) {
            assert_eq!(res.status(), StatusCode::OK, "{}", error_of(res).await);
        }
    }
}

/// Joins six humans, which deals a wolf, seer, doctor, sheriff, Cupid and a villager.
async fn join_six_humans(server: &Server) -> Vec<String> {
    let mut tokens = Vec::new();
    for i in 0..6 {
        let res = server.join_as(&format!("p{i}"), true).await;
        tokens.push(res.json::<JoinResponse>().await.unwrap().token);
    }
    tokens
}

/// Once the game starts: the roles by seat, and the two seats [`scripted`] kills on night 1.
async fn doomed(server: &Server) -> (Vec<String>, (u32, u32)) {
    let res = server.get(&format!("/spectate/events?key={KEY}")).await;
    let snapshot = read_events(res, |_| true).await.remove(0);
    let roles: Vec<String> = snapshot["roles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["role"].as_str().unwrap().to_owned())
        .collect();
    let mut doomed =
        (0..).filter(|&p: &u32| !["werewolf", "doctor"].contains(&roles[p as usize].as_str()));
    let pair = (doomed.next().unwrap(), doomed.next().unwrap());
    (roles, pair)
}

fn is_day_vote(event: &Value) -> bool {
    event["type"] == "turn_started" && event["kind"] == "day_vote"
}

#[tokio::test(flavor = "multi_thread")]
async fn dead_chat_reaches_only_the_dead_and_never_holds_the_day_open() {
    let dir = tempfile::tempdir().unwrap();
    // The day closes early only if the mute ghost doesn't count toward "everyone answered".
    let args = ["--discussion-secs=30", "--quiet-secs=1"];
    let server = Server::start(dir.path(), &args).await;
    let tokens = join_six_humans(&server).await;
    let mut streams = Vec::new();
    for token in &tokens {
        streams.push(tokio::spawn(read_events(
            server.watch(Some(token)).await,
            is_day_vote,
        )));
    }
    let audience = tokio::spawn(read_events(server.watch(None).await, is_day_vote));
    let spectator = server.get(&format!("/spectate/events?key={KEY}")).await;
    let spectator = tokio::spawn(read_timed_events(spectator, is_day_vote));
    assert_eq!(
        server.admin("/admin/start", Some(KEY)).await.status(),
        StatusCode::OK
    );

    let (roles, (a, b)) = doomed(&server).await;
    let ghost = |p: usize| p == a as usize || p == b as usize;

    let bots = tokens
        .iter()
        .zip(0..)
        .map(|(token, p)| scripted(&server, token, (a, b), p == b));
    let played = tokio::time::timeout(Duration::from_secs(60), join_all(bots))
        .await
        .expect("day 1 should reach the vote");

    let day_one = |turns: &[Turn]| -> Vec<TurnKind> {
        turns
            .iter()
            .filter(|t| t.view.phase == Phase::Day { n: 1 })
            .map(|t| t.kind)
            .collect()
    };
    for (p, turns) in played.iter().enumerate() {
        let kinds = day_one(turns);
        if ghost(p) {
            assert!(!kinds.is_empty(), "ghost {p} got dead chat turns");
            assert!(
                kinds.iter().all(|&k| k == TurnKind::DeadChat),
                "{p}: {kinds:?}"
            );
        } else {
            assert!(!kinds.contains(&TurnKind::DeadChat), "{p}: {kinds:?}");
            let views = serde_json::to_string(&turns.iter().map(|t| &t.view).collect::<Vec<_>>());
            assert!(
                !views.unwrap().contains(DEAD_CHAT),
                "dead chat in {p}'s poll view"
            );
        }
    }
    // The poster's later dead chat turns show its own message.
    let heard = played[a as usize].iter().any(|t| {
        t.view.private_log.iter().any(|e| {
            matches!(e, PrivateEvent::DeadSpoke { player, text } if *player == a && text.contains(DEAD_CHAT))
        })
    });
    assert!(heard, "the dead chat reached the ghost's poll view");

    for (p, stream) in streams.into_iter().enumerate() {
        let events = tokio::time::timeout(Duration::from_secs(10), stream)
            .await
            .unwrap()
            .unwrap();
        assert_no_leaks(Some(p), &roles, &events);
        let text = serde_json::to_string(&events).unwrap();
        assert_eq!(
            text.contains(DEAD_CHAT),
            ghost(p),
            "dead chat on {p}'s stream"
        );
        assert_eq!(
            text.contains("dead_chat"),
            ghost(p),
            "dead chat window on {p}'s stream"
        );
        let late = read_events(server.watch(Some(&tokens[p])).await, |_| true).await;
        assert_no_leaks(Some(p), &roles, &late);
        let late = serde_json::to_string(&late).unwrap();
        assert_eq!(
            late.contains(DEAD_CHAT),
            ghost(p),
            "dead chat in {p}'s snapshot"
        );
    }
    let events = tokio::time::timeout(Duration::from_secs(10), audience)
        .await
        .unwrap()
        .unwrap();
    assert_no_leaks(None, &roles, &events);
    let text = serde_json::to_string(&events).unwrap();
    assert!(!text.contains(DEAD_CHAT) && !text.contains("dead_chat"));

    let events = tokio::time::timeout(Duration::from_secs(10), spectator)
        .await
        .unwrap()
        .unwrap();
    let at = |kind: &str, open: bool| {
        events
            .iter()
            .find(|(_, e)| e["type"] == "chat_window" && e["kind"] == kind && e["open"] == open)
            .map(|(at, _)| *at)
            .unwrap_or_else(|| panic!("no {kind} window open={open}"))
    };
    let day = at("day_speak", false) - at("day_speak", true);
    assert!(
        day < Duration::from_secs(10),
        "the mute ghost held the day open: {day:?}"
    );
    at("dead_chat", true);
    at("dead_chat", false);
    let posted = events.iter().any(|(_, e)| {
        e["audience"] == "dead" && e["event"]["type"] == "dead_spoke" && e["event"]["player"] == a
    });
    assert!(posted, "spectators see the dead chat");

    let log = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .expect("game log written");
    assert!(
        std::fs::read_to_string(log).unwrap().contains(DEAD_CHAT),
        "the game log keeps the dead chat"
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn no_dead_chat_flag_keeps_the_dead_quiet() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path(), &["--no-dead-chat"]).await;
    let tokens = join_six_humans(&server).await;
    let spectator = server.get(&format!("/spectate/events?key={KEY}")).await;
    let spectator = tokio::spawn(read_events(spectator, is_day_vote));
    assert_eq!(
        server.admin("/admin/start", Some(KEY)).await.status(),
        StatusCode::OK
    );
    let (_, (a, b)) = doomed(&server).await;
    let (mut living, mut ghosts) = (Vec::new(), Vec::new());
    for (token, p) in tokens.iter().zip(0..) {
        let bot = scripted(&server, token, (a, b), false);
        if p == a || p == b {
            ghosts.push(bot);
        } else {
            living.push(bot);
        }
    }
    // A ghost returns only after a dead chat turn.
    tokio::select! {
        played = tokio::time::timeout(Duration::from_secs(60), join_all(living)) => {
            played.expect("day 1 should reach the vote");
        }
        _ = join_all(ghosts) => panic!("a ghost got a dead chat turn"),
    }
    let events = tokio::time::timeout(Duration::from_secs(10), spectator)
        .await
        .unwrap()
        .unwrap();
    let text = serde_json::to_string(&events).unwrap();
    assert!(!text.contains("dead_chat") && !text.contains("dead_spoke"));
    server.stop().await;
}

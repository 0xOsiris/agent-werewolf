//! End-to-end tests over real HTTP against an in-process server.

use std::{path::Path, time::Duration};

use clap::Parser;
use futures_util::future::join_all;
use rand::{SeedableRng, rngs::StdRng, seq::IndexedRandom};
use reqwest::{Client, Response, StatusCode};
use serde_json::Value;
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use werewolf_protocol::{
    ActRequest, Action, JoinRequest, JoinResponse, PlayerView, Poll, Team, Turn, TurnKind,
};
use werewolf_server::Config;

const KEY: &str = "test-key";
const SEED: u64 = 42;
/// Seat of the bot that never acts.
const SILENT: u32 = 5;
/// Markers that must only reach the right streams.
const REASONING: &str = "REASONING-MARKER";
const WOLF_CHAT: &str = "WOLF-CHAT-MARKER";

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
        let base = [
            "werewolf-server",
            "--no-mdns",
            "--pace-ms=0",
            &key,
            &seed,
            &log_dir,
        ];
        let config = Config::parse_from(base.iter().chain(args).copied());
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

/// Plays random legal moves, or never acts if `silent`, until the game ends. Spares the silent
/// bot so that it lives long enough to time out.
async fn bot(server: &Server, token: String, seed: u64, silent: bool) -> (Team, PlayerView) {
    let mut rng = StdRng::seed_from_u64(seed);
    loop {
        let res = server.poll(&token, 5).await;
        assert_eq!(res.status(), StatusCode::OK);
        match res.json::<Poll>().await.unwrap() {
            Poll::Wait => {}
            Poll::GameOver { winner, view } => return (winner, view),
            Poll::Turn(turn) if silent => {
                tokio::time::sleep(Duration::from_millis(turn.deadline_ms + 50)).await;
            }
            Poll::Turn(turn) => {
                let action = match turn.kind {
                    TurnKind::DaySpeak => Action::Speak {
                        text: format!("I am player {}. Trust me.", turn.view.you),
                    },
                    TurnKind::WolfChat => Action::Speak {
                        text: format!("{WOLF_CHAT}: let's eat someone"),
                    },
                    _ => {
                        let spared: Vec<u32> = turn
                            .candidates
                            .iter()
                            .copied()
                            .filter(|&c| c != SILENT)
                            .collect();
                        Action::Target {
                            target: spared.choose(&mut rng).copied(),
                        }
                    }
                };
                let res = server.act(&token, turn.turn_id, action).await;
                assert_eq!(res.status(), StatusCode::OK, "{}", error_of(res).await);
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

    let bots = tokens
        .into_iter()
        .zip(0..)
        .map(|(token, i)| bot(&server, token, i.into(), i == SILENT));
    let results = tokio::time::timeout(Duration::from_secs(120), join_all(bots))
        .await
        .expect("game should finish");

    let winner = results[0].0;
    assert!(
        results.iter().all(|(w, _)| *w == winner),
        "bots disagree on the winner"
    );
    assert!(
        results
            .iter()
            .all(|(_, view)| matches!(view.phase, werewolf_protocol::Phase::Over))
    );

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

    server.stop().await;
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

/// Asserts `viewer` (a seat, or `None` for the audience) may see `event`; `over` reveals roles.
fn assert_visible(viewer: Option<usize>, roles: &[String], over: bool, event: &Value) {
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
            let allowed = match role {
                Some("werewolf") => "wolves",
                Some("seer") => "seer",
                Some("doctor") => "doctor",
                _ => "nobody",
            };
            assert_eq!(event["audience"], allowed, "{viewer:?} saw {event}");
        }
        "turn_started" | "turn_ended" => assert!(sees_turn(event), "{viewer:?} saw {event}"),
        "roles" => sees_roles(&event["roles"], over),
        "snapshot" => {
            let over = !event["winner"].is_null();
            sees_roles(&event["roles"], over);
            for pending in event["pending"].as_array().unwrap() {
                assert!(sees_turn(pending), "{viewer:?} saw pending {pending}");
            }
            for inner in event["events"].as_array().unwrap() {
                assert_visible(viewer, roles, over, inner);
            }
        }
        _ => {}
    }
}

/// Checks a whole stream for leaks and, for wolves, that the pack's chat arrived.
fn assert_stream(viewer: Option<usize>, roles: &[String], events: &[Value]) {
    assert_eq!(events[0]["type"], "snapshot");
    let mut over = false;
    for event in events {
        assert_visible(viewer, roles, over, event);
        over |= is_game_over(event);
    }
    let text = serde_json::to_string(events).unwrap();
    assert!(!text.contains(REASONING), "reasoning leaked to {viewer:?}");
    let wolf = viewer.is_some_and(|me| roles[me] == "werewolf");
    assert_eq!(
        text.contains(WOLF_CHAT),
        wolf,
        "wolf chat visibility for {viewer:?}"
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
        .map(|(token, i)| bot(&server, token, i, false));
    tokio::time::timeout(Duration::from_secs(120), join_all(bots))
        .await
        .expect("game should finish");

    let summary = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().ends_with("-summary.json"))
        .unwrap();
    let summary: Value = serde_json::from_slice(&std::fs::read(summary).unwrap()).unwrap();
    let roles: Vec<String> = summary["players"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["role"].as_str().unwrap().to_owned())
        .collect();

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

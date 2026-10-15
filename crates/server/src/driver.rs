//! Runs one game: hands the engine's turns to players, waits, and feeds the answers back.

use std::{sync::Arc, time::Duration};

use tokio::time::Instant;
use werewolf_protocol::{Action, PlayerId, PrivateEvent, PublicEvent, TurnKind};

use crate::{
    chat::ChatWindow,
    engine::{Game, GameEvent, TurnSpec},
    game_log,
    spectator::SpectatorEvent,
    state::{AppState, Collected, millis},
};

/// Salts the chat jitter seed so it doesn't replay the engine's random stream.
const CHAT_SALT: u64 = u64::from_be_bytes(*b"chatjitr");
/// Salts the dead chat's jitter seed so it doesn't replay the living window's.
const DEAD_CHAT_SALT: u64 = u64::from_be_bytes(*b"deadchat");

pub(crate) async fn run(app: Arc<AppState>, mut game: Game, seed: u64) {
    pause(&app).await;
    let mut windows: u64 = 0;
    while !game.is_over() {
        if let Some(kind) = game.chat() {
            let chat_seed = (seed ^ CHAT_SALT).wrapping_add(windows);
            windows += 1;
            if !run_chat(&app, &mut game, kind, chat_seed).await {
                return;
            }
            continue;
        }
        let turns = game.turns();
        app.begin_turns(&game, &turns);
        if !app.wait_for_answers(&turns).await {
            return;
        }
        let mut answers = Vec::with_capacity(turns.len());
        let mut monologues = Vec::new();
        for collected in app.collect_answers(&turns) {
            if let Some(text) = collected.reasoning {
                monologues.push((collected.answer.player, collected.kind, text));
            }
            answers.push(collected.answer);
        }
        let events = game.submit(&answers);
        publish(&app, &turns, events, monologues).await;
    }

    let (summary, path) = app.summary(&game, seed);
    if let Some(path) = path {
        game_log::write_summary(&path, &summary).await;
    }
    app.finish(&game);
}

/// Runs one chat window, plus the dead chat, until it times out or goes quiet. False on shutdown.
async fn run_chat(app: &AppState, game: &mut Game, kind: TurnKind, seed: u64) -> bool {
    let players: Vec<PlayerId> = game.turns().iter().map(|t| t.player).collect();
    let ghosts = if app.config().no_dead_chat {
        Vec::new()
    } else {
        game.ghosts()
    };
    let length = app.chat_length(kind);
    let (rules, now) = (app.chat_rules(), Instant::now());
    let mut window = ChatWindow::open(app.chat_members(&players), rules, now, length, seed);
    // Only `window` decides when to close, so the early close counts only the living.
    let dead_seed = seed ^ DEAD_CHAT_SALT;
    let mut dead_chat = ChatWindow::open(app.chat_members(&ghosts), rules, now, length, dead_seed);
    app.publish(SpectatorEvent::ChatWindow {
        kind,
        open: true,
        ends_in_ms: millis(length),
    });
    if !ghosts.is_empty() {
        app.publish(SpectatorEvent::ChatWindow {
            kind: TurnKind::DeadChat,
            open: true,
            ends_in_ms: millis(length),
        });
    }
    loop {
        let now = Instant::now();
        for collected in app.take_answers(kind, &players) {
            post(app, game, &mut window, collected, now);
        }
        for collected in app.take_answers(TurnKind::DeadChat, &ghosts) {
            post(app, game, &mut dead_chat, collected, now);
        }
        if window.should_close(now) {
            break;
        }
        let due = window.due(now);
        if !due.is_empty() {
            app.issue_chat_turns(game, kind, &due, window.ends());
        }
        let due = dead_chat.due(now);
        if !due.is_empty() {
            app.issue_chat_turns(game, TurnKind::DeadChat, &due, window.ends());
        }
        let wake = dead_chat
            .next_turn()
            .into_iter()
            .fold(window.next_wake(), Instant::min);
        if !app.wait_for_answer(wake).await {
            return false;
        }
    }
    for collected in app.cancel_chat(kind, &players) {
        post(app, game, &mut window, collected, Instant::now());
    }
    app.publish(SpectatorEvent::ChatWindow {
        kind,
        open: false,
        ends_in_ms: 0,
    });
    if !ghosts.is_empty() {
        for collected in app.cancel_chat(TurnKind::DeadChat, &ghosts) {
            post(app, game, &mut dead_chat, collected, Instant::now());
        }
        app.publish(SpectatorEvent::ChatWindow {
            kind: TurnKind::DeadChat,
            open: false,
            ends_in_ms: 0,
        });
    }
    for event in game.close_chat() {
        app.publish(event.into());
    }
    true
}

/// Publishes a chat answer right away, unpaced, followed by its reasoning.
fn post(
    app: &AppState,
    game: &mut Game,
    window: &mut ChatWindow,
    collected: Collected,
    now: Instant,
) {
    let player = collected.answer.player;
    let text = match &collected.answer.action {
        Some(Action::Speak { text }) => text.as_str(),
        _ => "",
    };
    if window.answer(player, text.trim().is_empty(), now) {
        for event in game.speak(player, text) {
            app.publish(event.into());
        }
    }
    if let Some(text) = collected.reasoning {
        app.publish(SpectatorEvent::Reasoning {
            player,
            kind: collected.kind,
            text,
        });
    }
}

/// Publishes events one at a time, each followed by its actor's reasoning and a pause.
async fn publish(
    app: &AppState,
    turns: &[TurnSpec],
    events: Vec<GameEvent>,
    mut monologues: Vec<(PlayerId, TurnKind, String)>,
) {
    // Reasoning behind actions that left no event (silence, abstaining) goes first.
    let actors: Vec<PlayerId> = events.iter().filter_map(|e| actor(e, turns)).collect();
    for (player, kind, text) in monologues.extract_if(.., |(p, ..)| !actors.contains(p)) {
        app.publish(SpectatorEvent::Reasoning { player, kind, text });
    }
    for event in events {
        let actor = actor(&event, turns);
        app.publish(event.into());
        if let Some(i) = monologues.iter().position(|(p, ..)| Some(*p) == actor) {
            let (player, kind, text) = monologues.swap_remove(i);
            app.publish(SpectatorEvent::Reasoning { player, kind, text });
        }
        pause(app).await;
    }
}

/// The player whose action produced `event`, if any.
fn actor(event: &GameEvent, turns: &[TurnSpec]) -> Option<PlayerId> {
    let holder = |kind| turns.iter().find(|t| t.kind == kind).map(|t| t.player);
    match event {
        GameEvent::Public(PublicEvent::Spoke { player, .. })
        | GameEvent::Private(_, PrivateEvent::WolfSpoke { player, .. }) => Some(*player),
        GameEvent::Public(PublicEvent::Voted { voter, .. })
        | GameEvent::Private(_, PrivateEvent::WolfVoted { voter, .. }) => Some(*voter),
        GameEvent::Public(PublicEvent::Shot { sheriff, .. }) => Some(*sheriff),
        GameEvent::Private(_, PrivateEvent::Inspected { .. }) => holder(TurnKind::SeerInspect),
        GameEvent::Private(_, PrivateEvent::Protected { .. }) => holder(TurnKind::DoctorProtect),
        GameEvent::Private(_, PrivateEvent::Linked { .. }) => holder(TurnKind::CupidLink),
        _ => None,
    }
}

async fn pause(app: &AppState) {
    let pace = app.config().pace_ms;
    if pace > 0 {
        tokio::time::sleep(Duration::from_millis(pace)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Audience;

    #[test]
    fn reasoning_follows_the_event_its_actor_caused() {
        let turns = [TurnSpec {
            player: 4,
            kind: TurnKind::CupidLink,
            candidates: vec![1, 2],
        }];
        let shot = GameEvent::Public(PublicEvent::Shot {
            sheriff: 3,
            target: 1,
        });
        assert_eq!(
            actor(&shot, &turns),
            Some(3),
            "a firing sheriff is not holding fire"
        );
        let linked = GameEvent::Private(Audience::Cupid, PrivateEvent::Linked { a: 1, b: 2 });
        assert_eq!(actor(&linked, &turns), Some(4));
        assert_eq!(
            actor(&GameEvent::Public(PublicEvent::PeacefulNight), &turns),
            None
        );
    }
}

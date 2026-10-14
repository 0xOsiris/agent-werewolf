//! Runs one game: hands the engine's turns to players, waits, and feeds the answers back.

use std::{sync::Arc, time::Duration};

use werewolf_protocol::{PlayerId, PrivateEvent, PublicEvent, TurnKind};

use crate::{
    engine::{Game, GameEvent, TurnSpec},
    game_log,
    spectator::SpectatorEvent,
    state::AppState,
};

pub(crate) async fn run(app: Arc<AppState>, mut game: Game, seed: u64) {
    pause(&app).await;
    while !game.is_over() {
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
        GameEvent::Private(_, PrivateEvent::Inspected { .. }) => holder(TurnKind::SeerInspect),
        GameEvent::Private(_, PrivateEvent::Protected { .. }) => holder(TurnKind::DoctorProtect),
        _ => None,
    }
}

async fn pause(app: &AppState) {
    let pace = app.config().pace_ms;
    if pace > 0 {
        tokio::time::sleep(Duration::from_millis(pace)).await;
    }
}

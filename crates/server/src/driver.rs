//! Runs one game: hands the engine's turns to players, waits, and feeds the answers back.

use std::{sync::Arc, time::Duration};

use tokio::time::Instant;
use tracing::error;
use werewolf_engine::{Answer, Game, GameEvent, TurnSpec};
use werewolf_protocol::{Action, PlayerId, PrivateEvent, PublicEvent, TurnKind};
use werewolf_verify::{Transcript, transcript::BuildError};

use crate::{
    chat::ChatWindow,
    game_log,
    spectator::SpectatorEvent,
    stake::{self, During, Failure, OnChain, OpCursor, Stake},
    state::{AppState, Collected, millis},
};

/// Salts the chat jitter seed so it doesn't replay the engine's random stream.
const CHAT_SALT: u64 = u64::from_be_bytes(*b"chatjitr");

/// A staked game's chain side, owned by its driver.
pub(crate) struct Staked {
    pub stake: Arc<Stake>,
    pub game: Arc<OnChain>,
    pub cursor: OpCursor,
}

pub(crate) async fn run(app: Arc<AppState>, mut game: Game, seed: u64, mut staked: Option<Staked>) {
    match play(&app, &mut game, seed, staked.as_mut()).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(failure) => {
            match &staked {
                Some(staked) => app.halt(&staked.game, &failure),
                None => error!(%failure, "game stopped"),
            }
            return;
        }
    }

    let (summary, path) = app.summary(&game, seed);
    if let Some(path) = path {
        game_log::write_summary(&path, &summary).await;
    }
    // Taken before `finish`, after which a reset may clear the lobby.
    let transcript = app.finish_transcript(&game);
    app.finish(&game);
    if let (Some(staked), Some(transcript)) = (staked, transcript)
        && let Err(failure) = conclude(&app, &staked, transcript).await
    {
        app.halt(&staked.game, &failure);
    }
}

/// Plays until the game is over. False on shutdown.
async fn play(
    app: &AppState,
    game: &mut Game,
    seed: u64,
    mut staked: Option<&mut Staked>,
) -> Result<bool, Failure> {
    pause(app).await;
    let mut windows: u64 = 0;
    while !game.is_over() {
        if let Some(kind) = game.chat() {
            let chat_seed = (seed ^ CHAT_SALT).wrapping_add(windows);
            windows += 1;
            let step = app.begin_chat(kind);
            if !run_chat(app, game, kind, step, chat_seed, staked.as_deref()).await? {
                return Ok(false);
            }
            continue;
        }
        let turns = game.turns();
        let (step, round) = app.begin_round();
        app.begin_turns(game, &turns, step, round);
        if !app.wait_for_answers(&turns).await {
            return Ok(false);
        }
        let mut answers = Vec::with_capacity(turns.len());
        let mut monologues = Vec::new();
        for mut collected in app.collect_answers(&turns) {
            if let Some(text) = collected.reasoning.take() {
                monologues.push((collected.answer.player, collected.kind, text));
            }
            answers.push(collected);
        }
        let answers = match staked.as_deref_mut() {
            Some(staked) => settle_round(app, staked, round, answers).await?,
            None => answers.into_iter().map(|c| c.answer).collect(),
        };
        let events = game.submit(&answers);
        app.extend_log(&events).during("public log")?;
        publish(app, &turns, events, monologues).await;
    }
    Ok(true)
}

/// Checkpoints `round`, merges in its forced moves, and records it in the transcript.
async fn settle_round(
    app: &AppState,
    staked: &mut Staked,
    round: u32,
    collected: Vec<Collected>,
) -> Result<Vec<Answer>, Failure> {
    let forces = staked
        .stake
        .checkpoint(&staked.game, round, &mut staked.cursor)
        .await
        .during("checkpoint")?;
    let answers: Vec<(PlayerId, Option<(Action, stake::Signed)>)> = collected
        .into_iter()
        .map(|c| (c.answer.player, c.answer.action.zip(c.signed)))
        .collect();
    let applied = app
        .record_round(round, &forces, &answers, staked.game.enc_secret())
        .during("transcript")?;
    Ok(answers
        .iter()
        .zip(applied)
        .map(|(&(player, _), action)| Answer { player, action })
        .collect())
}

/// Saves and serves the transcript, then proposes the result on-chain.
async fn conclude(
    app: &AppState,
    staked: &Staked,
    transcript: Result<Transcript, BuildError>,
) -> Result<(), Failure> {
    let transcript = transcript.during("transcript")?;
    let json: Arc<[u8]> = serde_json::to_vec(&transcript).during("transcript")?.into();
    stake::write_atomic(&staked.game.transcript_path, &json)
        .await
        .during("saving the transcript")?;
    app.set_transcript(json);
    let uri = format!(
        "{}/v1/transcript?game_id={}",
        app.site().base_url,
        staked.game.id
    );
    let result = transcript.result;
    staked
        .stake
        .propose(
            &staked.game,
            result.winner_team,
            result.winners,
            result.transcript_root,
            uri,
        )
        .await
        .during("propose")
}

/// Runs one open chat window until it times out or goes quiet. False on shutdown.
async fn run_chat(
    app: &AppState,
    game: &mut Game,
    kind: TurnKind,
    step: u32,
    seed: u64,
    staked: Option<&Staked>,
) -> Result<bool, Failure> {
    let players: Vec<PlayerId> = game.turns().iter().map(|t| t.player).collect();
    let length = app.chat_length(kind);
    let mut window = ChatWindow::open(
        app.chat_members(&players),
        app.chat_rules(),
        Instant::now(),
        length,
        seed,
    );
    app.publish(SpectatorEvent::ChatWindow {
        kind,
        open: true,
        ends_in_ms: millis(length),
    });
    loop {
        let now = Instant::now();
        for collected in app.take_answers(kind, &players) {
            post(app, game, &mut window, collected, now, staked)?;
        }
        if window.should_close(now) {
            break;
        }
        let due = window.due(now);
        if !due.is_empty() {
            app.issue_chat_turns(game, kind, step, &due, window.ends());
        }
        if !app.wait_for_answer(window.next_wake()).await {
            return Ok(false);
        }
    }
    for collected in app.cancel_chat(kind, &players) {
        post(app, game, &mut window, collected, Instant::now(), staked)?;
    }
    app.publish(SpectatorEvent::ChatWindow {
        kind,
        open: false,
        ends_in_ms: 0,
    });
    let events = game.close_chat();
    app.extend_log(&events).during("public log")?;
    for event in events {
        app.publish(event.into());
    }
    Ok(true)
}

/// Publishes a chat answer right away, unpaced, followed by its reasoning. A staked post enters
/// the transcript and its receipt goes back to the waiting act.
fn post(
    app: &AppState,
    game: &mut Game,
    window: &mut ChatWindow,
    collected: Collected,
    now: Instant,
    staked: Option<&Staked>,
) -> Result<(), Failure> {
    let player = collected.answer.player;
    let text = match &collected.answer.action {
        Some(Action::Speak { text }) => text.as_str(),
        _ => "",
    };
    let mut receipt = None;
    if window.answer(player, text.trim().is_empty(), now) {
        if let (Some(staked), Some(signed)) = (staked, &collected.signed) {
            app.record_message(player, text, signed)
                .during("transcript")?;
            receipt = Some(
                staked
                    .stake
                    .receipt(player, signed)
                    .during("signing a receipt")?,
            );
        }
        let events = game.speak(player, text);
        app.extend_log(&events).during("public log")?;
        for event in events {
            app.publish(event.into());
        }
    }
    if let Some(reply) = collected.reply {
        // Fails only if the act stopped waiting; the message is recorded either way.
        let _ = reply.send(receipt);
    }
    if let Some(text) = collected.reasoning {
        app.publish(SpectatorEvent::Reasoning {
            player,
            kind: collected.kind,
            text,
        });
    }
    Ok(())
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
    use werewolf_engine::Audience;

    use super::*;

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

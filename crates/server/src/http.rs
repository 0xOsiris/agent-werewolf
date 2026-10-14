//! HTTP routes. See `PROTOCOL.md` for the player-facing contract.

use std::{borrow::Cow, convert::Infallible, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        DefaultBodyLimit, FromRequest, FromRequestParts, Path, Query, Request, State,
        rejection::QueryRejection,
    },
    http::{
        HeaderMap,
        header::{AUTHORIZATION, CONTENT_TYPE},
        request::Parts,
    },
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures_util::{Stream, StreamExt, stream};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::sync::broadcast::{Receiver, error::RecvError};
use tracing::{error, warn};
use werewolf_protocol::{
    ActRequest, JoinRequest, JoinResponse, PROTOCOL_VERSION, Poll, PublicEvent, Role,
};

use crate::{
    assets,
    error::HttpError,
    spectator::{Broadcast, SpectatorEvent, Viewer},
    state::{AppState, Subscription, Watcher},
};

const BODY_LIMIT: usize = 16 * 1024;
const DEFAULT_WAIT_SECS: u64 = 25;
const MAX_WAIT_SECS: u64 = 30;
const KEEP_ALIVE: Duration = Duration::from_secs(15);

pub fn router(app: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(|| async { assets::serve("index.html") }))
        .route("/play", get(|| async { assets::serve("play.html") }))
        .route("/static/{*path}", get(static_file))
        .route("/qr.svg", get(qr))
        .route("/healthz", get(healthz))
        .route("/v1/info", get(info))
        .route("/v1/join", post(join))
        .route("/v1/poll", get(poll))
        .route("/v1/act", post(act))
        .route("/v1/watch", get(watch))
        .route("/admin/start", post(admin_start))
        .route("/admin/reset", post(admin_reset))
        .route("/spectate/events", get(spectate))
        .fallback(|| async { HttpError::NotFound })
        .method_not_allowed_fallback(|| async { HttpError::MethodNotAllowed })
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(app)
}

/// `{}`, the success body for commands.
#[derive(Serialize)]
struct Empty {}

async fn static_file(Path(path): Path<String>) -> Response {
    assets::serve(&path)
}

async fn qr(State(app): State<Arc<AppState>>) -> Response {
    let svg = app.site().qr_svg.clone();
    ([(CONTENT_TYPE, "image/svg+xml")], svg).into_response()
}

#[derive(Serialize)]
struct Info {
    version: u32,
    play_url: String,
}

async fn info(State(app): State<Arc<AppState>>) -> Json<Info> {
    Json(Info {
        version: PROTOCOL_VERSION,
        play_url: app.site().play_url.clone(),
    })
}

async fn healthz() -> &'static str {
    "ok"
}

async fn join(
    State(app): State<Arc<AppState>>,
    JsonBody(req): JsonBody<JoinRequest>,
) -> Result<Json<JoinResponse>, HttpError> {
    app.join(req).map(Json)
}

#[derive(Deserialize)]
struct PollQuery {
    wait: Option<u64>,
}

async fn poll(
    State(app): State<Arc<AppState>>,
    Bearer(token): Bearer,
    query: Result<Query<PollQuery>, QueryRejection>,
) -> Result<Json<Poll>, HttpError> {
    let Query(query) = query?;
    let wait = query.wait.unwrap_or(DEFAULT_WAIT_SECS).min(MAX_WAIT_SECS);
    app.poll(&token, Duration::from_secs(wait)).await.map(Json)
}

async fn act(
    State(app): State<Arc<AppState>>,
    Bearer(token): Bearer,
    body: Result<JsonBody<ActRequest>, HttpError>,
) -> Result<Json<Empty>, HttpError> {
    app.act(&token, body.map(|JsonBody(req)| req))?;
    Ok(Json(Empty {}))
}

async fn admin_start(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Empty>, HttpError> {
    app.check_key(admin_key(&headers))?;
    app.start()?;
    Ok(Json(Empty {}))
}

async fn admin_reset(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Empty>, HttpError> {
    app.check_key(admin_key(&headers))?;
    app.reset()?;
    Ok(Json(Empty {}))
}

fn admin_key(headers: &HeaderMap) -> Option<&str> {
    headers.get("x-spectator-key")?.to_str().ok()
}

#[derive(Deserialize)]
struct KeyQuery {
    key: Option<String>,
}

async fn spectate(
    State(app): State<Arc<AppState>>,
    query: Result<Query<KeyQuery>, QueryRejection>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, HttpError> {
    let key = query.ok().and_then(|Query(q)| q.key);
    app.check_key(key.as_deref())?;
    event_stream(app, Watcher::Spectator)
}

#[derive(Deserialize)]
struct WatchQuery {
    token: Option<String>,
}

async fn watch(
    State(app): State<Arc<AppState>>,
    query: Result<Query<WatchQuery>, QueryRejection>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, HttpError> {
    let Query(query) = query?;
    let watcher = match query.token.filter(|t| !t.is_empty()) {
        Some(token) => Watcher::Player(token),
        None => Watcher::Audience,
    };
    event_stream(app, watcher)
}

fn event_stream(
    app: Arc<AppState>,
    watcher: Watcher,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, HttpError> {
    let subscription = app.subscribe(&watcher)?;
    let shutdown = app.shutdown().clone().cancelled_owned();
    let feed = Feed::new(app, watcher, subscription);
    let events = stream::unfold(feed, Feed::next).take_until(shutdown);
    Ok(Sse::new(events).keep_alive(KeepAlive::new().interval(KEEP_ALIVE)))
}

/// One subscriber's view of the broadcast, filtered for its viewer.
struct Feed {
    app: Arc<AppState>,
    watcher: Watcher,
    viewer: Viewer,
    /// Roles by seat, tracked from the stream itself so filtering follows event order.
    roles: Vec<Role>,
    over: bool,
    snapshot: Option<String>,
    events: Receiver<Arc<Broadcast>>,
    closed: bool,
}

impl Feed {
    fn new(app: Arc<AppState>, watcher: Watcher, subscription: Subscription) -> Self {
        Self {
            app,
            watcher,
            viewer: subscription.viewer,
            roles: subscription.roles,
            over: subscription.over,
            snapshot: Some(subscription.snapshot),
            events: subscription.events,
            closed: false,
        }
    }

    async fn next(mut self) -> Option<(Result<Event, Infallible>, Self)> {
        // serde_json escapes control characters, so payloads never contain raw newlines.
        if let Some(snapshot) = self.snapshot.take() {
            return Some((Ok(Event::default().data(snapshot)), self));
        }
        loop {
            if self.closed {
                return None;
            }
            match self.events.recv().await {
                Ok(message) => {
                    match &message.event {
                        SpectatorEvent::Roles { roles } => {
                            self.roles = roles.iter().map(|r| r.role).collect();
                        }
                        SpectatorEvent::Public {
                            event: PublicEvent::GameOver { .. },
                        } => self.over = true,
                        SpectatorEvent::Reset => {
                            self.roles.clear();
                            self.over = false;
                            // The token died with the lobby; a later player with this seat
                            // must not inherit the stream.
                            self.closed = matches!(self.viewer, Viewer::Player { .. });
                        }
                        _ => {}
                    }
                    let data: Arc<str> =
                        match message.event.filtered(self.viewer, &self.roles, self.over) {
                            None => continue,
                            Some(Cow::Borrowed(_)) => Arc::clone(&message.json),
                            Some(Cow::Owned(event)) => match serde_json::to_string(&event) {
                                Ok(json) => json.into(),
                                Err(err) => {
                                    error!(%err, "failed to serialize filtered event; skipping it");
                                    continue;
                                }
                            },
                        };
                    return Some((Ok(Event::default().data(&*data)), self));
                }
                Err(RecvError::Lagged(skipped)) => {
                    warn!(skipped, "event stream lagged; resending snapshot");
                    // Fails if a reset invalidated the token while we lagged.
                    let subscription = self.app.subscribe(&self.watcher).ok()?;
                    self.viewer = subscription.viewer;
                    self.roles = subscription.roles;
                    self.over = subscription.over;
                    self.events = subscription.events;
                    return Some((Ok(Event::default().data(subscription.snapshot)), self));
                }
                Err(RecvError::Closed) => return None,
            }
        }
    }
}

/// `Authorization: Bearer <token>`.
struct Bearer(String);

impl<S: Send + Sync> FromRequestParts<S> for Bearer {
    type Rejection = HttpError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let value = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        match value.and_then(|v| v.split_once(' ')) {
            Some((scheme, token)) if scheme.eq_ignore_ascii_case("bearer") => {
                Ok(Self(token.trim().to_owned()))
            }
            _ => Err(HttpError::Unauthorized),
        }
    }
}

/// JSON body that ignores `content-type`, so hand-rolled agents need not set it.
struct JsonBody<T>(T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for JsonBody<T> {
    type Rejection = HttpError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = Bytes::from_request(req, state).await?;
        Ok(Self(serde_json::from_slice(&bytes)?))
    }
}

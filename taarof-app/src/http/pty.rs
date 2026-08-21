//! Loopback PTY runtime adapter.
//!
//! This is the raw broker-backed WebSocket surface at
//! `GET /api/v1/tabs/{tab_id}/panes/{pane_id}/pty/ws`. Unlike the legacy
//! snapshot-diff `/attach` path (see [`super::pane_attach`]), this route speaks
//! the Taarof Remote Protocol terminal frames directly against a broker-owned
//! PTY: it sends a checkpoint-first attach, streams ordered `output` frames off
//! the broker's bounded [`crate::pty_broker::ReplayWindow`], and accepts signed
//! `input`/`resize` frames whose final dispatch is revalidated on the GTK main
//! thread before an `ack` is returned.
//!
//! The route is a privileged control surface, so it is gated on **both** a
//! loopback bind and `[http_control].enabled`; it never carries the bearer token
//! or the raw PTY to a non-loopback client. Panes that are not broker-owned
//! (legacy/already-running) are refused before upgrade and advertised with the
//! `legacy_snapshot` capability marker so the client falls back to `/attach`
//! rather than attempting a raw attach.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

use crate::pty_broker::{
    BrokerEpoch, BrokeredPane, CanonicalCheckpoint, OutputSeq, ResumeDecision,
};

/// Server-sent `output` frames are capped at 256 KiB by the protocol; broker
/// replay frames are read-chunk sized (8 KiB) so they never exceed this, but the
/// guard keeps the invariant local rather than assumed.
const PTY_OUTPUT_FRAME_MAX_BYTES: usize = 262_144;
/// Client-sent control frames (`input`/`resize`) are capped at 64 KiB.
const PTY_INPUT_FRAME_MAX_BYTES: usize = 65_536;
/// Signed control frames whose deadline has already passed are rejected; this is
/// the anti-replay bound the protocol common types describe.
const PTY_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// How often the blocking output pump wakes to re-check its shutdown flag. This
/// bounds how long the pump can outlive a disconnected socket.
const PTY_PUMP_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// The single grant generation issued for the lifetime of one control-enabled
/// connection. Grant rotation across devices is a later phase; for now every
/// `input`/`resize` frame must carry this generation, and a mismatch is refused
/// so a stale generation can never dispatch.
const PTY_GRANT_GENERATION: u64 = 1;

/// Outcome of resolving a `{tab_id, pane_id}` to a PTY adapter target. Produced
/// on the GTK main thread (where pane ownership lives) and handed back to the
/// loopback HTTP task; it only carries [`Send`] data, never a `!Send` pane
/// handle.
pub enum PtyAdapterResolution {
    /// The pane is broker-owned: a raw PTY attach is available.
    Brokered(PtyAdapterHandle),
    /// The pane exists but is not broker-owned (legacy/already-running). The
    /// adapter advertises the `legacy_snapshot` capability instead of attaching.
    /// This also covers headless/restored panes, which are not yet broker-owned.
    LegacySnapshot,
    /// No such pane in the requested tab.
    NotFound,
}

/// A broker-owned pane resolved for raw PTY attach. Holds the shared
/// [`BrokeredPane`] (which is `Send + Sync`) plus the dimensions the model
/// currently reports.
pub struct PtyAdapterHandle {
    pub pane: Arc<BrokeredPane>,
    pub cols: u16,
    pub rows: u16,
}

/// GTK-main-thread resolution of a pane to its PTY adapter target. Reads pane
/// ownership and, for broker-owned panes, clones the `Send` broker handle out of
/// the `!Send` [`crate::terminal::BrokerHandle`] so nothing `!Send` escapes.
pub(crate) fn resolve_pty_adapter_target(
    state: &crate::AppState,
    tab_id: u32,
    pane_id: u32,
) -> PtyAdapterResolution {
    for workspace in &state.workspaces {
        for tab in &workspace.tabs {
            if tab.id != tab_id {
                continue;
            }

            if let Some(leaf) = tab.panes.leaf(pane_id) {
                let Some(broker) = leaf.broker.as_ref() else {
                    // A live pane the broker does not own: never a raw attach.
                    return PtyAdapterResolution::LegacySnapshot;
                };
                let pane = Arc::clone(broker.pane());
                let (cols, rows) = pane.with_model(|model| {
                    let projection = model.projection();
                    (projection.cols as u16, projection.rows as u16)
                });
                return PtyAdapterResolution::Brokered(PtyAdapterHandle { pane, cols, rows });
            }

            if state.headless_pane(tab.id, pane_id).is_some() {
                // Restored/headless panes are represented by the legacy snapshot
                // capability until they are re-materialized under the broker.
                return PtyAdapterResolution::LegacySnapshot;
            }
        }
    }

    PtyAdapterResolution::NotFound
}

/// The signed gate that a control frame must still satisfy at final dispatch.
/// Carried across the bridge so the GTK main thread re-checks the epoch, grant
/// generation, and deadline authoritatively — not only the adapter task.
pub struct PtyDispatchGuard {
    pub expected_epoch: String,
    pub grant_generation: u64,
    pub deadline_ms: i64,
}

/// GTK-main-thread final dispatch of an `input` frame. Revalidates the grant,
/// deadline, pane liveness, and epoch (input is never carried across epochs) at
/// the point of dispatch, then writes to the PTY. Returns only after the bytes
/// have reached the kernel, so the caller may `ack` afterward.
pub(crate) fn dispatch_pty_input(
    state: &crate::AppState,
    tab_id: u32,
    pane_id: u32,
    guard: &PtyDispatchGuard,
    payload: &[u8],
) -> Result<(), String> {
    let pane = revalidate_control_target(state, tab_id, pane_id, guard)?;
    pane.write_input(payload)
        .map_err(|error| format!("pty input dispatch failed: {error}"))
}

/// GTK-main-thread final dispatch of a `resize` frame. Revalidates the grant,
/// deadline, pane liveness, and epoch, then resizes the PTY and headless model.
pub(crate) fn dispatch_pty_resize(
    state: &crate::AppState,
    tab_id: u32,
    pane_id: u32,
    guard: &PtyDispatchGuard,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let pane = revalidate_control_target(state, tab_id, pane_id, guard)?;
    pane.resize(cols, rows)
        .map_err(|error| format!("pty resize dispatch failed: {error}"))
}

/// Revalidate the full signed gate at the final main-thread dispatch: the grant
/// generation must still match, the deadline must still be live, the pane must
/// still be broker-owned, and its epoch must still match the connection's.
fn revalidate_control_target(
    state: &crate::AppState,
    tab_id: u32,
    pane_id: u32,
    guard: &PtyDispatchGuard,
) -> Result<Arc<BrokeredPane>, String> {
    if guard.grant_generation != PTY_GRANT_GENERATION {
        return Err("control grant generation is no longer valid".to_string());
    }
    if !deadline_is_live(guard.deadline_ms) {
        return Err("control frame deadline expired before dispatch".to_string());
    }
    resolve_live_broker_pane(state, tab_id, pane_id, &guard.expected_epoch)
}

fn resolve_live_broker_pane(
    state: &crate::AppState,
    tab_id: u32,
    pane_id: u32,
    expected_epoch: &str,
) -> Result<Arc<BrokeredPane>, String> {
    for workspace in &state.workspaces {
        for tab in &workspace.tabs {
            if tab.id != tab_id {
                continue;
            }
            let Some(leaf) = tab.panes.leaf(pane_id) else {
                continue;
            };
            let Some(broker) = leaf.broker.as_ref() else {
                return Err("pane is no longer broker-owned".to_string());
            };
            let pane = Arc::clone(broker.pane());
            if pane.epoch().to_string() != expected_epoch {
                return Err("pane epoch changed since attach".to_string());
            }
            return Ok(pane);
        }
    }
    Err("pane was not found".to_string())
}

// ── Bridge plumbing ──

async fn query_pty_adapter_resolution(
    state: &HttpState,
    tab_id: u32,
    pane_id: u32,
) -> Result<PtyAdapterResolution, StatusCode> {
    let (reply_tx, reply_rx) = oneshot::channel();
    state
        .bridge
        .try_send(HttpBridgeRequest::ResolvePtyAdapter {
            tab_id,
            pane_id,
            reply: reply_tx,
        })
        .map_err(bridge_overload_status)?;
    await_bridge_reply(reply_rx).await
}

async fn dispatch_input_over_bridge(
    state: &HttpState,
    tab_id: u32,
    pane_id: u32,
    guard: PtyDispatchGuard,
    payload: Vec<u8>,
) -> Result<(), String> {
    let (reply_tx, reply_rx) = oneshot::channel();
    state
        .bridge
        .try_send(HttpBridgeRequest::DispatchPtyInput {
            tab_id,
            pane_id,
            guard,
            payload,
            reply: reply_tx,
        })
        .map_err(|_| "pty control bridge is unavailable".to_string())?;
    await_bridge_reply(reply_rx)
        .await
        .map_err(|status| format!("pty control bridge failed with status {}", status.as_u16()))?
}

async fn dispatch_resize_over_bridge(
    state: &HttpState,
    tab_id: u32,
    pane_id: u32,
    guard: PtyDispatchGuard,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let (reply_tx, reply_rx) = oneshot::channel();
    state
        .bridge
        .try_send(HttpBridgeRequest::DispatchPtyResize {
            tab_id,
            pane_id,
            guard,
            cols,
            rows,
            reply: reply_tx,
        })
        .map_err(|_| "pty control bridge is unavailable".to_string())?;
    await_bridge_reply(reply_rx)
        .await
        .map_err(|status| format!("pty control bridge failed with status {}", status.as_u16()))?
}

// ── Route handler ──

#[derive(Deserialize, Default)]
pub(super) struct PtyWebSocketQuery {
    pub(super) token: Option<String>,
    pub(super) epoch: Option<String>,
    pub(super) output_seq: Option<String>,
}

pub(super) async fn tab_pane_pty_ws(
    AxumPath((tab_id, pane_id)): AxumPath<(u32, u32)>,
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(query): Query<PtyWebSocketQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(status) = check_ws_auth(&headers, query.token.as_deref(), &state.auth_token) {
        return status.into_response();
    }

    // Defence in depth: the raw PTY adapter is a write surface and must never be
    // reachable from a non-loopback bind, even if the read-only API was opted
    // into non-loopback exposure.
    if !state.bind_is_loopback {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "ok": false,
                "error": "the PTY adapter is only available on a loopback bind",
            })),
        )
            .into_response();
    }

    if check_control_enabled(&state).is_err() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "ok": false,
                "error": "the PTY adapter requires [http_control].enabled",
            })),
        )
            .into_response();
    }

    let handle = match query_pty_adapter_resolution(&state, tab_id, pane_id).await {
        Ok(PtyAdapterResolution::Brokered(handle)) => handle,
        Ok(PtyAdapterResolution::LegacySnapshot) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "ok": false,
                    "error": "pane is not broker-owned; use the legacy snapshot attach",
                    "capabilities": ["legacy_snapshot"],
                })),
            )
                .into_response();
        }
        Ok(PtyAdapterResolution::NotFound) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "ok": false,
                    "error": format!("pane {pane_id} was not found in tab {tab_id}"),
                })),
            )
                .into_response();
        }
        Err(status) => return status.into_response(),
    };

    let attach_permit = match Arc::clone(&state.pane_attach_slots).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({
                    "ok": false,
                    "error": "too many live pane attach clients",
                })),
            )
                .into_response();
        }
    };

    let requested_cursor = parse_requested_cursor(&query);

    ws.on_upgrade(move |socket| {
        handle_pty_socket(
            socket,
            state,
            tab_id,
            pane_id,
            handle,
            requested_cursor,
            attach_permit,
        )
    })
    .into_response()
}

fn parse_requested_cursor(query: &PtyWebSocketQuery) -> Option<(BrokerEpoch, OutputSeq)> {
    let epoch = query.epoch.as_deref()?.parse::<BrokerEpoch>().ok()?;
    let seq = query.output_seq.as_deref()?.parse::<u64>().ok()?;
    Some((epoch, OutputSeq::new(seq)))
}

async fn handle_pty_socket(
    mut socket: axum::extract::ws::WebSocket,
    state: HttpState,
    tab_id: u32,
    pane_id: u32,
    handle: PtyAdapterHandle,
    requested_cursor: Option<(BrokerEpoch, OutputSeq)>,
    _attach_permit: OwnedSemaphorePermit,
) {
    use axum::extract::ws::Message;

    let pane = handle.pane;
    let current_epoch = pane.epoch();
    let ctx = FrameContext {
        runtime_id: state.runtime_id.as_ref().clone(),
        session_name: crate::instance::session_name().unwrap_or_else(|| "default".to_string()),
        tab_id: tab_id.to_string(),
        pane_id: pane_id.to_string(),
        epoch: current_epoch.to_string(),
    };
    let started = Instant::now();

    // Register for output wakeups *before* computing the first cursor so live
    // output produced during attach still wakes the loop; the retained window is
    // read authoritatively via `resume` so nothing is missed or duplicated.
    let subscription = pane.subscribe();
    let notify = Arc::new(Notify::new());
    let closed = Arc::new(AtomicBool::new(false));
    // Signals the blocking output pump to stop and drop its broker subscription
    // when this connection ends, so neither the pump thread nor the phantom
    // subscriber outlive the socket.
    let pump_shutdown = Arc::new(AtomicBool::new(false));
    spawn_output_pump(
        subscription,
        Arc::clone(&notify),
        Arc::clone(&closed),
        Arc::clone(&pump_shutdown),
    );

    stream_pty_frames(
        &mut socket,
        &state,
        tab_id,
        pane_id,
        &pane,
        &ctx,
        started,
        requested_cursor,
        &notify,
        &closed,
    )
    .await;

    // Stop the pump promptly on disconnect: it wakes within its poll interval,
    // sees this flag, and drops the broker subscription (which the broker then
    // prunes on its next read fan-out).
    pump_shutdown.store(true, Ordering::Release);
    let _ = socket.send(Message::Close(None)).await;
}

/// Drive the checkpoint-first attach and the live streaming loop until the
/// socket closes, the client errors, or the child exits.
#[allow(clippy::too_many_arguments)] // Streaming loop threads its explicit connection state.
async fn stream_pty_frames(
    socket: &mut axum::extract::ws::WebSocket,
    state: &HttpState,
    tab_id: u32,
    pane_id: u32,
    pane: &Arc<BrokeredPane>,
    ctx: &FrameContext,
    started: Instant,
    requested_cursor: Option<(BrokerEpoch, OutputSeq)>,
    notify: &Arc<Notify>,
    closed: &Arc<AtomicBool>,
) {
    use axum::extract::ws::Message;

    let current_epoch = pane.epoch();
    let mut cursor = OutputSeq::zero();
    if send_initial_frames(socket, pane, ctx, requested_cursor, &mut cursor)
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            biased;
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(payload))) => {
                        if handle_client_frame(
                            socket,
                            state,
                            tab_id,
                            pane_id,
                            pane,
                            ctx,
                            payload.as_str(),
                        )
                        .await
                        .is_err()
                        {
                            break;
                        }
                    }
                    Some(Ok(Message::Binary(_))) => {
                        let _ = socket
                            .send(Message::text(
                                ctx.error_frame("unsupported_frame", "binary frames are not supported")
                                    .to_string(),
                            ))
                            .await;
                        break;
                    }
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                }
            }
            _ = notify.notified() => {
                if flush_output(socket, pane, current_epoch, ctx, &mut cursor)
                    .await
                    .is_err()
                {
                    break;
                }
                if closed.load(Ordering::Acquire) {
                    // The child has exited and the final output has been flushed.
                    break;
                }
            }
            _ = tokio::time::sleep(PTY_HEARTBEAT_INTERVAL) => {
                let monotonic_ms = started.elapsed().as_millis() as u64;
                if socket
                    .send(Message::text(ctx.heartbeat_frame(monotonic_ms).to_string()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}

/// Bridge the broker's blocking subscriber channel into an async wakeup. Each
/// output chunk (and the terminal EOF) notifies the streaming loop, which then
/// reads the authoritative window via `resume`. The channel payloads themselves
/// are ignored here — they exist only to wake the loop.
///
/// The channel `recv` is polled with a timeout so the pump can observe
/// `shutdown` and exit promptly when the connection ends, dropping the broker
/// subscription on the way out; without this the blocking thread and its phantom
/// subscriber would linger for the child's remaining lifetime.
fn spawn_output_pump(
    subscription: std::sync::mpsc::Receiver<Vec<u8>>,
    notify: Arc<Notify>,
    closed: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) {
    use std::sync::mpsc::RecvTimeoutError;

    tokio::task::spawn_blocking(move || loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        match subscription.recv_timeout(PTY_PUMP_POLL_INTERVAL) {
            Ok(_) => notify.notify_one(),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                closed.store(true, Ordering::Release);
                notify.notify_one();
                break;
            }
        }
    });
}

async fn send_initial_frames(
    socket: &mut axum::extract::ws::WebSocket,
    pane: &Arc<BrokeredPane>,
    ctx: &FrameContext,
    requested_cursor: Option<(BrokerEpoch, OutputSeq)>,
    cursor: &mut OutputSeq,
) -> Result<(), ()> {
    match requested_cursor {
        Some((epoch, ack_seq)) => {
            let decision = pane.with_replay(|window| window.resume(epoch, ack_seq));
            match decision {
                ResumeDecision::Replay { frames, .. } => {
                    // The requested cursor is still covered: resume in place with
                    // ordered output frames and claim continuity (no checkpoint).
                    for frame in frames {
                        send_output_frame(socket, ctx, frame.seq, &frame.payload).await?;
                        *cursor = frame.seq;
                    }
                    Ok(())
                }
                ResumeDecision::AlreadyAtLatest { latest_seq, .. } => {
                    *cursor = latest_seq;
                    Ok(())
                }
                ResumeDecision::ReplayGap { .. }
                | ResumeDecision::WrongEpoch { .. }
                | ResumeDecision::FutureCursor { .. } => {
                    // Continuity cannot be honoured; supply a fresh checkpoint
                    // rather than claiming a resume that would drop or reorder
                    // output.
                    send_checkpoint(socket, pane, ctx, cursor).await
                }
            }
        }
        None => send_checkpoint(socket, pane, ctx, cursor).await,
    }
}

/// Send a checkpoint-first frame capturing the current canonical screen. The
/// latest sequence is captured *before* the checkpoint so the cursor never runs
/// ahead of the state the checkpoint represents: a chunk that lands between the
/// two reads is then re-sent as output (a bounded, harmless overlap) rather than
/// dropped.
async fn send_checkpoint(
    socket: &mut axum::extract::ws::WebSocket,
    pane: &Arc<BrokeredPane>,
    ctx: &FrameContext,
    cursor: &mut OutputSeq,
) -> Result<(), ()> {
    use axum::extract::ws::Message;

    let latest = pane.with_replay(|window| window.latest_seq());
    let checkpoint = pane.with_model(|model| model.checkpoint());
    let frame = ctx.checkpoint_frame(latest, &checkpoint);
    socket
        .send(Message::text(frame.to_string()))
        .await
        .map_err(|_| ())?;
    *cursor = latest;
    Ok(())
}

async fn flush_output(
    socket: &mut axum::extract::ws::WebSocket,
    pane: &Arc<BrokeredPane>,
    current_epoch: BrokerEpoch,
    ctx: &FrameContext,
    cursor: &mut OutputSeq,
) -> Result<(), ()> {
    let decision = pane.with_replay(|window| window.resume(current_epoch, *cursor));
    match decision {
        ResumeDecision::Replay { frames, .. } => {
            for frame in frames {
                send_output_frame(socket, ctx, frame.seq, &frame.payload).await?;
                *cursor = frame.seq;
            }
            Ok(())
        }
        ResumeDecision::AlreadyAtLatest { .. } => Ok(()),
        ResumeDecision::ReplayGap { .. }
        | ResumeDecision::WrongEpoch { .. }
        | ResumeDecision::FutureCursor { .. } => {
            // The window evicted output faster than it could be streamed: emit a
            // fresh checkpoint instead of pretending the stream was continuous.
            send_checkpoint(socket, pane, ctx, cursor).await
        }
    }
}

async fn send_output_frame(
    socket: &mut axum::extract::ws::WebSocket,
    ctx: &FrameContext,
    seq: OutputSeq,
    payload: &[u8],
) -> Result<(), ()> {
    use axum::extract::ws::Message;

    for chunk in payload.chunks(PTY_OUTPUT_FRAME_MAX_BYTES) {
        let frame = ctx.output_frame(seq, chunk);
        socket
            .send(Message::text(frame.to_string()))
            .await
            .map_err(|_| ())?;
    }
    Ok(())
}

async fn handle_client_frame(
    socket: &mut axum::extract::ws::WebSocket,
    state: &HttpState,
    tab_id: u32,
    pane_id: u32,
    pane: &Arc<BrokeredPane>,
    ctx: &FrameContext,
    payload: &str,
) -> Result<(), ()> {
    use axum::extract::ws::Message;

    let frame: ClientFrame = match serde_json::from_str(payload) {
        Ok(frame) => frame,
        Err(_) => {
            let _ = socket
                .send(Message::text(
                    ctx.error_frame("invalid_frame", "frame was not a valid terminal frame")
                        .to_string(),
                ))
                .await;
            return Err(());
        }
    };

    match frame {
        ClientFrame::Input {
            epoch,
            grant_generation,
            deadline_ms,
            payload_base64,
            ..
        } => {
            let bytes = match validate_control_frame(
                ctx,
                &epoch,
                &grant_generation,
                deadline_ms,
                Some(&payload_base64),
            ) {
                Ok(bytes) => bytes.unwrap_or_default(),
                Err((code, message)) => {
                    let _ = socket
                        .send(Message::text(ctx.error_frame(code, &message).to_string()))
                        .await;
                    return Ok(());
                }
            };
            let guard = PtyDispatchGuard {
                expected_epoch: ctx.epoch.clone(),
                grant_generation: PTY_GRANT_GENERATION,
                deadline_ms,
            };
            match dispatch_input_over_bridge(state, tab_id, pane_id, guard, bytes).await {
                Ok(()) => {
                    // Acknowledge only after the input reached the kernel; the
                    // ack pins the output position observed post-dispatch.
                    let ack_seq = pane.with_replay(|window| window.latest_seq());
                    socket
                        .send(Message::text(ctx.ack_frame(ack_seq).to_string()))
                        .await
                        .map_err(|_| ())
                }
                Err(message) => {
                    let _ = socket
                        .send(Message::text(
                            ctx.error_frame("input_dispatch_failed", &message)
                                .to_string(),
                        ))
                        .await;
                    Ok(())
                }
            }
        }
        ClientFrame::Resize {
            epoch,
            grant_generation,
            deadline_ms,
            cols,
            rows,
            ..
        } => {
            if let Err((code, message)) =
                validate_control_frame(ctx, &epoch, &grant_generation, deadline_ms, None)
            {
                let _ = socket
                    .send(Message::text(ctx.error_frame(code, &message).to_string()))
                    .await;
                return Ok(());
            }
            let (cols, rows) = match validate_resize_dimensions(cols, rows) {
                Ok(dims) => dims,
                Err((code, message)) => {
                    let _ = socket
                        .send(Message::text(ctx.error_frame(code, &message).to_string()))
                        .await;
                    return Ok(());
                }
            };
            let guard = PtyDispatchGuard {
                expected_epoch: ctx.epoch.clone(),
                grant_generation: PTY_GRANT_GENERATION,
                deadline_ms,
            };
            match dispatch_resize_over_bridge(state, tab_id, pane_id, guard, cols, rows).await {
                Ok(()) => {
                    let ack_seq = pane.with_replay(|window| window.latest_seq());
                    socket
                        .send(Message::text(ctx.ack_frame(ack_seq).to_string()))
                        .await
                        .map_err(|_| ())
                }
                Err(message) => {
                    let _ = socket
                        .send(Message::text(
                            ctx.error_frame("resize_dispatch_failed", &message)
                                .to_string(),
                        ))
                        .await;
                    Ok(())
                }
            }
        }
        // Client acks are accepted as valid protocol but need no server action.
        ClientFrame::Ack => Ok(()),
    }
}

/// Validate the epoch/grant/deadline that gate a signed control frame, and
/// decode its payload when one is present. Returns the decoded bytes (for
/// `input`) or `None` (for `resize`), or a `(code, message)` error to surface.
fn validate_control_frame(
    ctx: &FrameContext,
    epoch: &str,
    grant_generation: &str,
    deadline_ms: i64,
    payload_base64: Option<&str>,
) -> Result<Option<Vec<u8>>, (&'static str, String)> {
    if epoch != ctx.epoch {
        return Err((
            "epoch_mismatch",
            "control input is never carried across connection epochs".to_string(),
        ));
    }
    if grant_generation != PTY_GRANT_GENERATION.to_string() {
        return Err((
            "grant_generation_mismatch",
            "control grant generation does not match this connection".to_string(),
        ));
    }
    if !deadline_is_live(deadline_ms) {
        return Err((
            "deadline_expired",
            "control frame deadline has already passed".to_string(),
        ));
    }

    match payload_base64 {
        Some(encoded) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| {
                    (
                        "invalid_payload",
                        "input payload was not valid base64".to_string(),
                    )
                })?;
            if bytes.len() > PTY_INPUT_FRAME_MAX_BYTES {
                return Err((
                    "frame_too_large",
                    format!("input exceeds the {PTY_INPUT_FRAME_MAX_BYTES} byte control limit"),
                ));
            }
            Ok(Some(bytes))
        }
        None => Ok(None),
    }
}

fn validate_resize_dimensions(cols: i64, rows: i64) -> Result<(u16, u16), (&'static str, String)> {
    if !(2..=500).contains(&cols) || !(2..=200).contains(&rows) {
        return Err((
            "invalid_resize",
            "resize dimensions are outside the supported bounds".to_string(),
        ));
    }
    Ok((cols as u16, rows as u16))
}

fn deadline_is_live(deadline_ms: i64) -> bool {
    if deadline_ms <= 0 {
        return false;
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0);
    deadline_ms >= now_ms
}

// ── Frame types ──

/// Incoming client frames. Only the fields the adapter acts on are captured;
/// the remaining protocol base fields are ignored on deserialize.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind")]
enum ClientFrame {
    #[serde(rename = "input")]
    Input {
        epoch: String,
        grant_generation: String,
        deadline_ms: i64,
        payload_base64: String,
    },
    #[serde(rename = "resize")]
    Resize {
        epoch: String,
        grant_generation: String,
        deadline_ms: i64,
        cols: i64,
        rows: i64,
    },
    // Client acks acknowledge output consumption; the broker window is
    // self-bounding, so the sequence value is intentionally not read.
    #[serde(rename = "ack")]
    Ack,
}

/// The immutable frame identity for one connection, stamped onto every
/// server-sent frame so it validates against `protocol/schemas/terminal-frame`.
struct FrameContext {
    runtime_id: String,
    session_name: String,
    tab_id: String,
    pane_id: String,
    epoch: String,
}

impl FrameContext {
    fn base(&self, kind: &str) -> serde_json::Map<String, Value> {
        let mut map = serde_json::Map::new();
        map.insert("kind".to_string(), json!(kind));
        map.insert(
            "protocol_version".to_string(),
            json!({ "major": 1, "minor": 0 }),
        );
        map.insert("runtime_id".to_string(), json!(self.runtime_id));
        map.insert("session_name".to_string(), json!(self.session_name));
        map.insert("tab_id".to_string(), json!(self.tab_id));
        map.insert("pane_id".to_string(), json!(self.pane_id));
        map.insert("epoch".to_string(), json!(self.epoch));
        map
    }

    fn checkpoint_frame(&self, output_seq: OutputSeq, checkpoint: &CanonicalCheckpoint) -> Value {
        let mut map = self.base("checkpoint");
        map.insert(
            "output_seq".to_string(),
            json!(output_seq.get().to_string()),
        );
        map.insert("cols".to_string(), json!(checkpoint.cols));
        map.insert("rows".to_string(), json!(checkpoint.rows));
        map.insert(
            "checkpoint".to_string(),
            json!({
                "ansi_base64": base64::engine::general_purpose::STANDARD.encode(&checkpoint.ansi),
                "byte_count": checkpoint.ansi.len(),
                "state_hash": checkpoint.state_hash,
            }),
        );
        Value::Object(map)
    }

    fn output_frame(&self, output_seq: OutputSeq, payload: &[u8]) -> Value {
        let mut map = self.base("output");
        map.insert(
            "output_seq".to_string(),
            json!(output_seq.get().to_string()),
        );
        map.insert(
            "payload_base64".to_string(),
            json!(base64::engine::general_purpose::STANDARD.encode(payload)),
        );
        map.insert("byte_count".to_string(), json!(payload.len()));
        Value::Object(map)
    }

    fn ack_frame(&self, ack_output_seq: OutputSeq) -> Value {
        let mut map = self.base("ack");
        map.insert(
            "ack_output_seq".to_string(),
            json!(ack_output_seq.get().to_string()),
        );
        Value::Object(map)
    }

    fn heartbeat_frame(&self, monotonic_ms: u64) -> Value {
        let mut map = self.base("heartbeat");
        map.insert("monotonic_ms".to_string(), json!(monotonic_ms));
        Value::Object(map)
    }

    fn error_frame(&self, code: &str, message: &str) -> Value {
        let mut map = self.base("error");
        map.insert(
            "error".to_string(),
            json!({
                "code": code,
                "message": message,
            }),
        );
        Value::Object(map)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{test_router_with_gates, BridgeReceiver, HttpBridgeRequest};
    use super::{PtyAdapterHandle, PtyAdapterResolution};
    use crate::pty_broker::{BrokeredPane, PtyBroker, SpawnSpec};
    use axum::Router;
    use base64::Engine as _;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use tokio_tungstenite::tungstenite;

    enum BridgeMode {
        Broker(Arc<BrokeredPane>),
        Legacy,
        NotFound,
    }

    fn spawn_broker(command: &str, cols: u16, rows: u16) -> Arc<BrokeredPane> {
        Arc::new(
            PtyBroker::spawn(SpawnSpec {
                argv: vec!["/bin/sh".to_string(), "-c".to_string(), command.to_string()],
                cwd: None,
                env: Vec::new(),
                cols,
                rows,
            })
            .expect("broker should spawn a fake child over a real PTY"),
        )
    }

    async fn serve(app: Router) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("test listener should bind");
        let addr = listener
            .local_addr()
            .expect("test listener should have a local address");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("test server runs");
        });
        addr
    }

    fn service_bridge(mut rx: BridgeReceiver, mode: BridgeMode) {
        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                match request {
                    HttpBridgeRequest::ResolvePtyAdapter { reply, .. } => {
                        let resolution = match &mode {
                            BridgeMode::Broker(pane) => {
                                PtyAdapterResolution::Brokered(PtyAdapterHandle {
                                    pane: Arc::clone(pane),
                                    cols: 80,
                                    rows: 24,
                                })
                            }
                            BridgeMode::Legacy => PtyAdapterResolution::LegacySnapshot,
                            BridgeMode::NotFound => PtyAdapterResolution::NotFound,
                        };
                        let _ = reply.send(resolution);
                    }
                    HttpBridgeRequest::DispatchPtyInput {
                        guard,
                        payload,
                        reply,
                        ..
                    } => {
                        let result = match &mode {
                            BridgeMode::Broker(pane)
                                if guard.expected_epoch == pane.epoch().to_string() =>
                            {
                                pane.write_input(&payload)
                                    .map_err(|error| error.to_string())
                            }
                            _ => Err("pane epoch changed since attach".to_string()),
                        };
                        let _ = reply.send(result);
                    }
                    HttpBridgeRequest::DispatchPtyResize {
                        guard,
                        cols,
                        rows,
                        reply,
                        ..
                    } => {
                        let result = match &mode {
                            BridgeMode::Broker(pane)
                                if guard.expected_epoch == pane.epoch().to_string() =>
                            {
                                pane.resize(cols, rows).map_err(|error| error.to_string())
                            }
                            _ => Err("pane epoch changed since attach".to_string()),
                        };
                        let _ = reply.send(result);
                    }
                    _ => {}
                }
            }
        });
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("multi-thread runtime should build")
    }

    async fn connect(
        url: &str,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tungstenite::Error,
    > {
        tokio_tungstenite::connect_async(url)
            .await
            .map(|(socket, _response)| socket)
    }

    async fn next_frame<S>(socket: &mut S) -> Value
    where
        S: StreamExt<Item = Result<tungstenite::Message, tungstenite::Error>> + Unpin,
    {
        let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("a frame should arrive before timeout")
            .expect("the socket should yield a frame")
            .expect("the frame should be a valid message")
            .into_text()
            .expect("the frame should be text");
        serde_json::from_str(&message).expect("the frame should be valid json")
    }

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_millis() as i64
    }

    fn input_frame(epoch: &str, payload: &[u8]) -> Value {
        json!({
            "kind": "input",
            "protocol_version": { "major": 1, "minor": 0 },
            "runtime_id": "00000000-0000-4000-8000-000000000001",
            "session_name": "default",
            "tab_id": "1",
            "pane_id": "2",
            "epoch": epoch,
            "input_seq": "1",
            "grant_generation": "1",
            "nonce": "00000000-0000-4000-8000-000000000002",
            "deadline_ms": now_ms() + 5000,
            "payload_base64": base64::engine::general_purpose::STANDARD.encode(payload),
            "byte_count": payload.len(),
        })
    }

    fn wait_for(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if condition() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_until_model_contains(pane: &Arc<BrokeredPane>, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let visible = pane.with_model(|model| model.projection().visible_text.join("\n"));
            if visible.contains(needle) {
                return;
            }
            assert!(Instant::now() < deadline, "model never rendered {needle:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn pty_ws_requires_auth() {
        runtime().block_on(async {
            let (app, _rx) = test_router_with_gates(true, true);
            let addr = serve(app).await;
            let url = format!("ws://{addr}/api/v1/tabs/1/panes/2/pty/ws");
            let error = connect(&url)
                .await
                .expect_err("missing token must be refused");
            match error {
                tungstenite::Error::Http(response) => assert_eq!(response.status(), 401),
                other => panic!("expected an HTTP 401, got {other:?}"),
            }
        });
    }

    #[test]
    fn pty_ws_refuses_when_control_disabled() {
        runtime().block_on(async {
            let (app, _rx) = test_router_with_gates(false, true);
            let addr = serve(app).await;
            let url = format!("ws://{addr}/api/v1/tabs/1/panes/2/pty/ws?token=secret");
            let error = connect(&url)
                .await
                .expect_err("control-disabled must be refused");
            match error {
                tungstenite::Error::Http(response) => assert_eq!(response.status(), 403),
                other => panic!("expected an HTTP 403, got {other:?}"),
            }
        });
    }

    #[test]
    fn pty_ws_refuses_non_loopback_bind() {
        runtime().block_on(async {
            let (app, _rx) = test_router_with_gates(true, false);
            let addr = serve(app).await;
            let url = format!("ws://{addr}/api/v1/tabs/1/panes/2/pty/ws?token=secret");
            let error = connect(&url)
                .await
                .expect_err("a non-loopback bind must refuse the raw PTY adapter");
            match error {
                tungstenite::Error::Http(response) => assert_eq!(response.status(), 403),
                other => panic!("expected an HTTP 403, got {other:?}"),
            }
        });
    }

    #[test]
    fn pty_ws_legacy_pane_advertises_legacy_snapshot() {
        runtime().block_on(async {
            let (app, rx) = test_router_with_gates(true, true);
            service_bridge(rx, BridgeMode::Legacy);
            let addr = serve(app).await;
            let url = format!("ws://{addr}/api/v1/tabs/1/panes/2/pty/ws?token=secret");
            let error = connect(&url)
                .await
                .expect_err("a non-broker pane must not raw-attach");
            match error {
                tungstenite::Error::Http(response) => {
                    assert_eq!(response.status(), 409);
                    let body = response
                        .body()
                        .as_ref()
                        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                        .unwrap_or_default();
                    assert!(
                        body.contains("legacy_snapshot"),
                        "legacy pane must advertise the legacy_snapshot capability, body was {body}"
                    );
                }
                other => panic!("expected an HTTP 409, got {other:?}"),
            }
        });
    }

    #[test]
    fn pty_ws_missing_pane_is_not_found() {
        runtime().block_on(async {
            let (app, rx) = test_router_with_gates(true, true);
            service_bridge(rx, BridgeMode::NotFound);
            let addr = serve(app).await;
            let url = format!("ws://{addr}/api/v1/tabs/1/panes/2/pty/ws?token=secret");
            let error = connect(&url)
                .await
                .expect_err("a missing pane is not found");
            match error {
                tungstenite::Error::Http(response) => assert_eq!(response.status(), 404),
                other => panic!("expected an HTTP 404, got {other:?}"),
            }
        });
    }

    #[test]
    fn pty_ws_sends_checkpoint_first() {
        runtime().block_on(async {
            let pane = spawn_broker("printf 'CHECKPOINT-MARK'; sleep 30", 80, 24);
            wait_until_model_contains(&pane, "CHECKPOINT-MARK");

            let (app, rx) = test_router_with_gates(true, true);
            service_bridge(rx, BridgeMode::Broker(Arc::clone(&pane)));
            let addr = serve(app).await;
            let url = format!("ws://{addr}/api/v1/tabs/1/panes/2/pty/ws?token=secret");
            let mut socket = connect(&url).await.expect("attach should succeed");

            let frame = next_frame(&mut socket).await;
            assert_eq!(
                frame["kind"],
                json!("checkpoint"),
                "first frame must be a checkpoint"
            );
            assert_eq!(frame["epoch"], json!(pane.epoch().to_string()));
            let ansi = base64::engine::general_purpose::STANDARD
                .decode(
                    frame["checkpoint"]["ansi_base64"]
                        .as_str()
                        .expect("ansi is a string"),
                )
                .expect("checkpoint ansi decodes");
            // The checkpoint is a canonical redraw; feeding it into a fresh model
            // must reconstruct the rendered screen.
            let cols = frame["cols"].as_u64().unwrap() as usize;
            let rows = frame["rows"].as_u64().unwrap() as usize;
            let mut reconstructed = crate::pty_broker::TerminalStateModel::new(cols, rows);
            reconstructed.feed(&ansi);
            let visible = reconstructed.projection().visible_text.join("\n");
            assert!(
                visible.contains("CHECKPOINT-MARK"),
                "the checkpoint must reconstruct the rendered screen, saw {visible:?}"
            );
        });
    }

    #[test]
    fn pty_ws_input_frame_is_acked_and_reaches_the_child() {
        runtime().block_on(async {
            let pane = spawn_broker("IFS= read -r line; printf 'ECHO:%s\\n' \"$line\"", 80, 24);

            let (app, rx) = test_router_with_gates(true, true);
            service_bridge(rx, BridgeMode::Broker(Arc::clone(&pane)));
            let addr = serve(app).await;
            let url = format!("ws://{addr}/api/v1/tabs/1/panes/2/pty/ws?token=secret");
            let mut socket = connect(&url).await.expect("attach should succeed");

            let first = next_frame(&mut socket).await;
            assert_eq!(first["kind"], json!("checkpoint"));

            let epoch = pane.epoch().to_string();
            socket
                .send(tungstenite::Message::text(
                    input_frame(&epoch, b"hello-input\n").to_string(),
                ))
                .await
                .expect("input frame should send");

            let mut saw_ack = false;
            let mut output = String::new();
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline && !(saw_ack && output.contains("ECHO:hello-input")) {
                let frame = next_frame(&mut socket).await;
                match frame["kind"].as_str() {
                    Some("ack") => saw_ack = true,
                    Some("output") => {
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(frame["payload_base64"].as_str().unwrap())
                            .expect("output decodes");
                        output.push_str(&String::from_utf8_lossy(&bytes));
                    }
                    _ => {}
                }
            }
            assert!(saw_ack, "input must be acknowledged after final dispatch");
            assert!(
                output.contains("ECHO:hello-input"),
                "the child must observe the dispatched input; output was {output:?}"
            );
        });
    }

    #[test]
    fn pty_ws_input_with_wrong_epoch_is_refused_and_never_dispatched() {
        runtime().block_on(async {
            let pane = spawn_broker("IFS= read -r line; printf 'ECHO:%s\\n' \"$line\"", 80, 24);

            let (app, rx) = test_router_with_gates(true, true);
            service_bridge(rx, BridgeMode::Broker(Arc::clone(&pane)));
            let addr = serve(app).await;
            let url = format!("ws://{addr}/api/v1/tabs/1/panes/2/pty/ws?token=secret");
            let mut socket = connect(&url).await.expect("attach should succeed");
            assert_eq!(next_frame(&mut socket).await["kind"], json!("checkpoint"));

            let wrong_epoch = "abababab-abab-4bab-8bab-abababababab";
            socket
                .send(tungstenite::Message::text(
                    input_frame(wrong_epoch, b"should-not-run\n").to_string(),
                ))
                .await
                .expect("frame should send");

            let frame = next_frame(&mut socket).await;
            assert_eq!(frame["kind"], json!("error"));
            assert_eq!(frame["error"]["code"], json!("epoch_mismatch"));
        });
    }

    #[test]
    fn pty_ws_disconnect_releases_pump_and_subscriber() {
        runtime().block_on(async {
            // A child that echoes each line lets us force a post-disconnect read,
            // which is when the broker prunes a dead subscriber.
            let pane = spawn_broker(
                "while IFS= read -r line; do printf 'ECHO:%s\\n' \"$line\"; done",
                80,
                24,
            );
            let baseline = pane.subscriber_count();

            let (app, rx) = test_router_with_gates(true, true);
            service_bridge(rx, BridgeMode::Broker(Arc::clone(&pane)));
            let addr = serve(app).await;
            let url = format!("ws://{addr}/api/v1/tabs/1/panes/2/pty/ws?token=secret");
            let mut socket = connect(&url).await.expect("attach should succeed");
            assert_eq!(next_frame(&mut socket).await["kind"], json!("checkpoint"));

            // The attach registered exactly one live subscriber.
            assert!(
                wait_for(Duration::from_secs(5), || pane.subscriber_count()
                    == baseline + 1),
                "the attach must register one broker subscriber"
            );

            // Disconnect the client.
            let _ = socket.close(None).await;
            drop(socket);

            // Once the pump observes shutdown and drops its receiver, the next read
            // fan-out prunes the phantom subscriber. Drive output until the count
            // returns to baseline (this also proves the pump released the channel).
            let returned = wait_for(Duration::from_secs(5), || {
                let _ = pane.write_input(b"ping\n");
                pane.subscriber_count() == baseline
            });
            assert!(
                returned,
                "the pump must drop its subscription on disconnect so the broker prunes it"
            );
        });
    }

    #[test]
    fn pty_ws_input_with_expired_deadline_is_refused() {
        runtime().block_on(async {
            let pane = spawn_broker("IFS= read -r line; printf 'ECHO:%s\\n' \"$line\"", 80, 24);

            let (app, rx) = test_router_with_gates(true, true);
            service_bridge(rx, BridgeMode::Broker(Arc::clone(&pane)));
            let addr = serve(app).await;
            let url = format!("ws://{addr}/api/v1/tabs/1/panes/2/pty/ws?token=secret");
            let mut socket = connect(&url).await.expect("attach should succeed");
            assert_eq!(next_frame(&mut socket).await["kind"], json!("checkpoint"));

            let epoch = pane.epoch().to_string();
            let mut frame = input_frame(&epoch, b"too-late\n");
            frame["deadline_ms"] = json!(now_ms() - 1000);
            socket
                .send(tungstenite::Message::text(frame.to_string()))
                .await
                .expect("frame should send");

            let response = next_frame(&mut socket).await;
            assert_eq!(response["kind"], json!("error"));
            assert_eq!(response["error"]["code"], json!("deadline_expired"));
        });
    }

    #[test]
    fn pty_ws_resume_within_window_replays_without_a_checkpoint() {
        runtime().block_on(async {
            let pane = spawn_broker("printf 'LINE-ONE\\n'; IFS= read -r _; sleep 30", 80, 24);
            wait_until_model_contains(&pane, "LINE-ONE");
            let cursor = pane.with_replay(|window| window.latest_seq());

            // Produce more output strictly after the captured cursor.
            pane.write_input(b"\n").expect("unblock the child's read");
            let deadline = Instant::now() + Duration::from_secs(5);
            while pane.with_replay(|window| window.latest_seq()) <= cursor {
                assert!(Instant::now() < deadline, "no post-cursor output arrived");
                std::thread::sleep(Duration::from_millis(10));
            }

            let (app, rx) = test_router_with_gates(true, true);
            service_bridge(rx, BridgeMode::Broker(Arc::clone(&pane)));
            let addr = serve(app).await;
            let epoch = pane.epoch().to_string();
            let url = format!(
                "ws://{addr}/api/v1/tabs/1/panes/2/pty/ws?token=secret&epoch={epoch}&output_seq={}",
                cursor.get()
            );
            let mut socket = connect(&url).await.expect("resume should succeed");

            let frame = next_frame(&mut socket).await;
            assert_eq!(
                frame["kind"],
                json!("output"),
                "an in-window resume must replay output rather than reset with a checkpoint"
            );
        });
    }

    #[test]
    fn pty_ws_resume_beyond_window_supplies_a_fresh_checkpoint() {
        runtime().block_on(async {
            let pane = spawn_broker("printf 'GAP-MARK'; sleep 30", 80, 24);
            wait_until_model_contains(&pane, "GAP-MARK");

            let (app, rx) = test_router_with_gates(true, true);
            service_bridge(rx, BridgeMode::Broker(Arc::clone(&pane)));
            let addr = serve(app).await;
            let epoch = pane.epoch().to_string();
            // A cursor far ahead of the latest sequence cannot be honoured.
            let url = format!(
                "ws://{addr}/api/v1/tabs/1/panes/2/pty/ws?token=secret&epoch={epoch}&output_seq=999999"
            );
            let mut socket = connect(&url).await.expect("attach should succeed");

            let frame = next_frame(&mut socket).await;
            assert_eq!(
                frame["kind"],
                json!("checkpoint"),
                "a resume the window cannot cover must supply a fresh checkpoint"
            );
        });
    }
}

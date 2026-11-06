use super::*;

#[derive(Debug)]
pub enum SpotInput {
    Trade(Trade),
    Heartbeat(u64),
}

pub async fn enqueue_spot(state: &AppState, symbol: Symbol, sender: &mpsc::Sender<SpotInput>, input: SpotInput) {
    if is_terminal(state, Exchange::Coinbase, Market::Spot, symbol).await {
        return;
    }
    match sender.try_send(input) {
        Ok(()) => (),
        Err(mpsc::error::TrySendError::Full(_)) => {
            mark_buffer_exhausted(state, Exchange::Coinbase, Market::Spot, symbol).await
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            reject_source(
                state,
                Exchange::Coinbase,
                Market::Spot,
                symbol,
                "spot worker ended unexpectedly; source paused".into(),
            )
            .await
        }
    }
}

/// Prefix of the coordinator's Retry detail for an authoritative ID hole.
const SPOT_HOLE: &str = "spot history has not closed the ID gap";

/// Why the last attempt of a bounded episode did not finish. Only `Transient`
/// episodes back off and retry indefinitely; the others fail closed as before.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum AttemptEnd {
    /// Network/transport, request timeout, HTTP 408/429/5xx, lagging/empty history.
    Transient,
    /// Authoritative history overlapped but skipped an ID.
    Hole,
    /// Malformed authoritative data or a non-retryable HTTP status.
    Invalid,
}

/// Classify a history fetch error through its actual source type.
pub(super) fn classify_fetch_error(error: &anyhow::Error) -> AttemptEnd {
    match error.downcast_ref::<reqwest::Error>() {
        Some(e) if e.is_decode() => AttemptEnd::Invalid,
        Some(e) => match e.status() {
            Some(s) if s == reqwest::StatusCode::TOO_MANY_REQUESTS
                || s == reqwest::StatusCode::REQUEST_TIMEOUT
                || s.is_server_error() => AttemptEnd::Transient,
            Some(_) => AttemptEnd::Invalid,
            None if e.is_connect() || e.is_timeout() || e.is_request() || e.is_body() => AttemptEnd::Transient,
            None => AttemptEnd::Invalid,
        },
        // Parse/validation failures of authoritative rows (not transport).
        None => AttemptEnd::Invalid,
    }
}

pub(crate) async fn run_spot_worker_with_base(
    state: Arc<AppState>,
    symbol: Symbol,
    rx: mpsc::Receiver<SpotInput>,
    base: String,
) {
    let fetch_state = state.clone();
    run_spot_worker_with_fetch(state, symbol, rx, move |anchor| {
        let state = fetch_state.clone();
        let base = base.clone();
        async move {
            fetch_recent_with_spot_base(
                &state,
                Exchange::Coinbase,
                Market::Spot,
                symbol,
                &anchor,
                &base,
            )
            .await
        }
    })
    .await
}

// One FIFO consumer owns both the committed cursor and the fixed recovery target.
// The 20-attempt/30-second budget covers one bounded recovery episode, including
// heartbeats received while fetching; a moving target cannot restart it. An episode
// that ends without progress for transient reasons backs off (capped) and starts
// another from the same cursor; an authoritative ID hole stays terminal.
pub(super) async fn run_spot_worker_with_fetch<F, Fut>(
    state: Arc<AppState>,
    symbol: Symbol,
    rx: mpsc::Receiver<SpotInput>,
    fetch: F,
) where
    F: FnMut(RecoveryAnchor) -> Fut,
    Fut: Future<Output = Result<Vec<Trade>>>,
{
    // The shard (runtime owner) carries the highest known execution above the
    // committed cursor across socket workers, so a successor that sees only a
    // lower valid prefix keeps the predecessor's recovery obligation.
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, symbol).clone();
    let mut known = shard.engine.lock().await.spot_known_frontier.unwrap_or(0);
    spot_worker(state, symbol, rx, fetch, &mut known).await;
    let mut engine = shard.engine.lock().await;
    let known = engine.spot_known_frontier.unwrap_or(0).max(known);
    engine.spot_known_frontier = owes_recovery(&engine, known).then_some(known);
}

/// A known execution is a replay obligation only above an existing boundary:
/// the ingested cursor, else a witnessed bootstrap origin (the origin itself is
/// not an obligation), else an uncommitted closed-minute recovery boundary.
fn owes_recovery(engine: &crate::ingestion::SymbolState, known: u64) -> bool {
    match engine.last_trade_id.as_deref() {
        Some(id) => id.parse::<u64>().is_ok_and(|c| known > c),
        None => match engine.spot_bootstrap_id {
            Some(origin) => known > origin,
            None => engine.recovery_from_ms.is_some() && known > 0,
        },
    }
}

fn note_known(known: &mut u64, trade: &Trade) {
    if let Ok(id) = trade.id.parse::<u64>() {
        *known = (*known).max(id);
    }
}

async fn spot_worker<F, Fut>(
    state: Arc<AppState>,
    symbol: Symbol,
    mut rx: mpsc::Receiver<SpotInput>,
    mut fetch: F,
    known: &mut u64,
) where
    F: FnMut(RecoveryAnchor) -> Fut,
    Fut: Future<Output = Result<Vec<Trade>>>,
{
    let exchange = Exchange::Coinbase;
    let market = Market::Spot;
    let shard = crate::pick_shard(&state, exchange, market, symbol).clone();
    if shard.recovery.read().await.needs_backfill() {
        while rx.recv().await.is_some() {}
        return;
    }
    let (mut startup, handed_over) = {
        let engine = shard.engine.lock().await;
        (engine.last_trade_ms.or(engine.recovery_from_ms).is_some(), owes_recovery(&engine, *known))
    };
    // A handed-over obligation starts recovery at once, without new input.
    let mut pending = handed_over.then_some(SpotInput::Heartbeat(*known));
    let mut budget: Option<(Instant, usize)> = None;
    while let Some(input) = match pending.take() {
        Some(input) => Some(input),
        None => rx.recv().await,
    } {
        if shard.recovery.read().await.needs_backfill() {
            break;
        }
        let (committed_cursor, origin, from_ms) = {
            let engine = shard.engine.lock().await;
            (
                engine.last_trade_id.as_deref().and_then(|id| id.parse::<u64>().ok()),
                engine.spot_bootstrap_id,
                engine.last_trade_ms.or(engine.recovery_from_ms),
            )
        };
        let cursor = committed_cursor.or(origin);
        let id = match &input {
            SpotInput::Trade(t) => match t.id.parse::<u64>() {
                Ok(id) => id,
                Err(_) => {
                    reject_source(&state, exchange, market, symbol, "invalid spot trade ID".into()).await;
                    break;
                }
            },
            SpotInput::Heartbeat(id) => *id,
        };
        // Every observed execution (trade or heartbeat) is a known frontier.
        *known = (*known).max(id);
        if !startup && cursor.is_none() && from_ms.is_none() {
            if let SpotInput::Trade(trade) = input {
                if let Err(error) = ingest_trade(&state, exchange, market, symbol, trade).await {
                    reject_source(&state, exchange, market, symbol, format!("live ingestion failed: {error}")).await;
                    break;
                }
                mark_live(&state, exchange, market, symbol).await;
            } else if let SpotInput::Heartbeat(id) = input {
                // Do not move the origin on a later heartbeat: executions after
                // this witnessed ID belong to the new source's coverage.
                shard.engine.lock().await.spot_bootstrap_id.get_or_insert(id);
            }
            continue;
        }
        if committed_cursor.is_none() && origin.is_some_and(|start| id <= start) {
            // Pre-origin matches and stale heartbeats cannot establish a later
            // baseline or be mistaken for newly observed executions.
            continue;
        }
        // A heartbeat is the exchange's latest execution, not the next
        // execution to ingest. Even one missing tail trade needs REST.
        let gap = cursor.is_some_and(|old| match input {
            SpotInput::Heartbeat(_) => id > old,
            SpotInput::Trade(_) => id > old.saturating_add(1),
        });
        if !startup && !gap {
            if let SpotInput::Trade(trade) = input {
                if let Err(error) = ingest_trade(&state, exchange, market, symbol, trade).await {
                    reject_source(&state, exchange, market, symbol, format!("live ingestion failed: {error}")).await;
                    break;
                }
                if committed_cursor.is_none() {
                    mark_live(&state, exchange, market, symbol).await;
                }
            }
            continue;
        }
        let anchor = RecoveryAnchor { cursor: cursor.map(|id| id.to_string()), from_ms: from_ms.unwrap_or(0) };
        let mut coordinator = RecoveryCoordinator::new(anchor.clone(), RECOVERY_CHANNEL_CAPACITY);
        coordinator.set_spot_target(id);
        if let SpotInput::Trade(trade) = input {
            coordinator.push_ws_coalescing(trade);
        }
        let mut diagnostics = RecoveryDiagnostics {
            state: "recovering".into(),
            durable_cursor: committed_cursor.map(|id| id.to_string()),
            durable_from_ms: from_ms,
            detail: Some(if committed_cursor.is_none() && origin.is_some() {
                format!("spot gap after witnessed heartbeat origin {}; origin was not ingested", origin.unwrap())
            } else {
                "spot ID gap; waiting for authoritative history".into()
            }),
            ..RecoveryDiagnostics::default()
        };
        if !store_recovery(&shard, diagnostics.clone()).await {
            break;
        }
        *shard.stream_error.write().await = Some("recovering: spot trade ID gap".into());
        // Consecutive bounded episodes that ended without progress for transient
        // reasons (transport errors, lagging/empty history). Nothing is ingested
        // between episodes, so the anchor/cursor and fixed target stay valid.
        let mut outage = 0_u32;
        loop {
            let (deadline, attempts) = budget.get_or_insert_with(|| (Instant::now() + RECOVERY_DEADLINE, 0));
            let mut recovered = false;
            let mut end = AttemptEnd::Transient;
            let mut last_error = String::new();
            'attempts: while *attempts < RECOVERY_ATTEMPTS && Instant::now() < *deadline {
                *attempts += 1;
                diagnostics.attempts = *attempts;
                diagnostics.buffered_trades = coordinator.buffered_len();
                if !store_recovery(&shard, diagnostics.clone()).await {
                    return;
                }
                let request = fetch(anchor.clone());
                tokio::pin!(request);
                let result = loop {
                    tokio::select! {
                        input = rx.recv() => match input {
                            Some(SpotInput::Trade(trade)) => {
                                note_known(known, &trade);
                                coordinator.push_ws_coalescing(trade)
                            }
                            Some(SpotInput::Heartbeat(id)) => *known = (*known).max(id),
                            None => return, // socket closed: cancel pending REST, retain committed cursor
                        },
                        result = &mut request => break Some(result),
                        _ = tokio::time::sleep_until(*deadline) => break None,
                    }
                };
                let Some(result) = result else {
                    end = AttemptEnd::Transient;
                    last_error = "history request outlived the episode deadline".into();
                    break 'attempts;
                };
                let decision = match result {
                    Ok(rows) => {
                        diagnostics.rest_count = rows.len();
                        diagnostics.rest_earliest_ms = rows.iter().map(|t| t.timestamp_ms).min();
                        diagnostics.rest_latest_ms = rows.iter().map(|t| t.timestamp_ms).max();
                        coordinator.observe_batch(rows, None)
                    }
                    Err(error) => {
                        end = classify_fetch_error(&error);
                        RecoveryDecision::Retry(format!(
                            "history {} error: {error}",
                            if end == AttemptEnd::Transient { "transport" } else { "response" }
                        ))
                    }
                };
                match decision {
                    RecoveryDecision::Ready(trades) => {
                        if let Err(error) = validate_spot_handoff(&anchor, &trades) {
                            reject_source(&state, exchange, market, symbol, error.to_string()).await;
                            return;
                        }
                        if shard.recovery.read().await.needs_backfill() {
                            return;
                        }
                        for trade in &trades {
                            if let Err(error) = ingest_trade(&state, exchange, market, symbol, trade.clone()).await {
                                reject_source(&state, exchange, market, symbol, format!("spot replay failed: {error}"))
                                    .await;
                                return;
                            }
                        }
                        shard.engine.lock().await.remember_replay(&trades);
                        recovered = true;
                        break;
                    }
                    RecoveryDecision::NeedsBackfill(detail) => {
                        reject_source(&state, exchange, market, symbol, detail).await;
                        return;
                    }
                    RecoveryDecision::Retry(detail) => {
                        if detail.starts_with(SPOT_HOLE) {
                            end = AttemptEnd::Hole;
                        } else if !detail.starts_with("history ") {
                            end = AttemptEnd::Transient; // lagging/empty/behind-cursor history
                        }
                        last_error = detail.chars().take(300).collect();
                        diagnostics.detail = Some(detail);
                        diagnostics.buffered_trades = coordinator.buffered_len();
                        if !store_recovery(&shard, diagnostics.clone()).await {
                            return;
                        }
                    }
                }
                let sleep = tokio::time::sleep(RECOVERY_RETRY_DELAY);
                tokio::pin!(sleep);
                loop {
                    tokio::select! {
                        input = rx.recv() => match input {
                            Some(SpotInput::Trade(trade)) => {
                                note_known(known, &trade);
                                coordinator.push_ws_coalescing(trade)
                            }
                            Some(SpotInput::Heartbeat(id)) => *known = (*known).max(id),
                            None => return,
                        },
                        _ = &mut sleep => break,
                        _ = tokio::time::sleep_until(*deadline) => break 'attempts,
                    }
                }
            }
            if recovered {
                break;
            }
            let attempts = *attempts;
            if end != AttemptEnd::Transient {
                reject_source(
                    &state,
                    exchange,
                    market,
                    symbol,
                    format!(
                        "spot recovery budget exhausted after {attempts} attempts; historical backfill required; last: {last_error}"
                    ),
                )
                .await;
                return;
            }
            // Transient: stay recovering (never live), back off, then start a new
            // bounded episode from the same durable cursor without needing new input.
            outage = outage.saturating_add(1);
            let wait = outage_backoff(outage);
            diagnostics.state = "recovering".into();
            diagnostics.buffered_trades = coordinator.buffered_len();
            diagnostics.detail = Some(format!(
                "spot history temporarily unavailable; episode {outage} ended after {attempts} attempts without progress; automatic retry in {}s; last: {}",
                wait.as_secs(),
                last_error
            ));
            if !store_recovery(&shard, diagnostics.clone()).await {
                return;
            }
            let until = Instant::now() + wait;
            loop {
                tokio::select! {
                    input = rx.recv() => match input {
                        Some(SpotInput::Trade(trade)) => {
                                note_known(known, &trade);
                                coordinator.push_ws_coalescing(trade)
                            }
                        Some(SpotInput::Heartbeat(id)) => *known = (*known).max(id),
                        None => return,
                    },
                    _ = tokio::time::sleep_until(until) => break,
                }
            }
            if shard.recovery.read().await.needs_backfill() {
                return;
            }
            budget = None;
        }
        startup = false;
        let committed =
            shard.engine.lock().await.last_trade_id.as_deref().and_then(|id| id.parse::<u64>().ok()).unwrap_or(0);
        if *known > committed {
            pending = Some(SpotInput::Heartbeat(*known));
        } else {
            budget = None;
            mark_live(&state, exchange, market, symbol).await;
        }
    }
}

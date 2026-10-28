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
// The 20-attempt/30-second budget covers the entire recovery episode, including
// heartbeats received while fetching; a moving target cannot restart the budget.
pub(super) async fn run_spot_worker_with_fetch<F, Fut>(
    state: Arc<AppState>,
    symbol: Symbol,
    mut rx: mpsc::Receiver<SpotInput>,
    mut fetch: F,
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
    let mut startup = {
        let engine = shard.engine.lock().await;
        engine.last_trade_ms.or(engine.recovery_from_ms).is_some()
    };
    let mut pending = None;
    let mut budget: Option<(Instant, usize)> = None;
    let mut observed_heartbeat = 0_u64;
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
        if matches!(input, SpotInput::Heartbeat(_)) {
            observed_heartbeat = observed_heartbeat.max(id);
        }
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
        let (deadline, attempts) = budget.get_or_insert_with(|| (Instant::now() + RECOVERY_DEADLINE, 0));
        let anchor = RecoveryAnchor {
            cursor: cursor.map(|id| id.to_string()),
            from_ms: from_ms.unwrap_or(0),
            through_ms: match &input {
                SpotInput::Trade(t) => t.timestamp_ms,
                _ => 0,
            },
        };
        let mut coordinator = RecoveryCoordinator::new(anchor.clone(), RECOVERY_CHANNEL_CAPACITY);
        coordinator.set_spot_target(id);
        if let SpotInput::Trade(trade) = input {
            if let Err(error) = coordinator.push_ws(trade) {
                reject_source(&state, exchange, market, symbol, error.to_string()).await;
                break;
            }
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
        let mut recovered = false;
        while *attempts < RECOVERY_ATTEMPTS && Instant::now() < *deadline {
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
                            if let Err(error) = coordinator.push_ws(trade) { reject_source(&state, exchange, market, symbol, error.to_string()).await; return; }
                        },
                        Some(SpotInput::Heartbeat(id)) => observed_heartbeat = observed_heartbeat.max(id),
                        None => return, // socket closed: cancel pending REST, retain committed cursor
                    },
                    result = &mut request => break result,
                    _ = tokio::time::sleep_until(*deadline) => { reject_source(&state, exchange, market, symbol, "spot recovery deadline exhausted; historical backfill required".into()).await; return; }
                }
            };
            let decision = match result {
                Ok(rows) => {
                    diagnostics.rest_count = rows.len();
                    diagnostics.rest_earliest_ms = rows.iter().map(|t| t.timestamp_ms).min();
                    diagnostics.rest_latest_ms = rows.iter().map(|t| t.timestamp_ms).max();
                    coordinator.observe_batch(rows, None)
                }
                Err(error) => RecoveryDecision::Retry(format!("history transport error: {error}")),
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
                            if let Err(error) = coordinator.push_ws(trade) { reject_source(&state, exchange, market, symbol, error.to_string()).await; return; }
                        },
                        Some(SpotInput::Heartbeat(id)) => observed_heartbeat = observed_heartbeat.max(id),
                        None => return,
                    },
                    _ = &mut sleep => break,
                    _ = tokio::time::sleep_until(*deadline) => { reject_source(&state, exchange, market, symbol, "spot recovery deadline exhausted; historical backfill required".into()).await; return; }
                }
            }
        }
        if !recovered {
            reject_source(
                &state,
                exchange,
                market,
                symbol,
                format!("spot recovery budget exhausted after {attempts} attempts; historical backfill required"),
            )
            .await;
            break;
        }
        startup = false;
        let committed =
            shard.engine.lock().await.last_trade_id.as_deref().and_then(|id| id.parse::<u64>().ok()).unwrap_or(0);
        if observed_heartbeat > committed {
            pending = Some(SpotInput::Heartbeat(observed_heartbeat));
        } else {
            budget = None;
            mark_live(&state, exchange, market, symbol).await;
        }
    }
}

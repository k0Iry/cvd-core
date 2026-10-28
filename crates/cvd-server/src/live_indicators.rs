//! Per-scope live indicator hubs.
//!
//! A hub exists only while at least one WebSocket viewer subscribes to a
//! `(exchange, market, symbol, tf)` scope; all viewers of that scope share one
//! set of runner live sessions (one per indicator × mode), not one per viewer
//! or per tick.
//!
//! Ingestion hands bars to a hub with a non-blocking `try_send`; it never
//! awaits a plugin. If the bounded queue overflows the hub resynchronises
//! (bumps its generation and re-opens sessions from the shard snapshot). The
//! hub converts consecutive bars into `append` / `replace` deltas with a strict
//! per-session sequence; on `sequence_gap`, `unknown_session`, crash, timeout or
//! restart the session is re-opened by replaying the bounded window from the
//! cached shard history — never by resending full history every tick.

use crate::indicators::{input_bar, mode_str, IndicatorManager};
use crate::models::Bar;
use crate::types::{Exchange, Market, Symbol, Tf};
use crate::AppState;
use cvd_indicator_sdk::Output;
use cvd_indicator_sdk::{LiveInit, LiveOp, LiveUpdate, Mode, PluginError, Scope};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

pub type HubKey = (Exchange, Market, Symbol, Tf);

/// Bounded queue between ingestion and a hub.
pub const HUB_QUEUE: usize = 256;
/// Fan-out buffer to viewers (lagging viewers are disconnected like base bars).
pub const HUB_FANOUT: usize = 256;
const HUB_TICK: Duration = Duration::from_secs(2);

pub struct Hub {
    events: mpsc::Sender<(Bar, u64)>,
    out: broadcast::Sender<Arc<str>>,
    subscribers: AtomicUsize,
    resync: AtomicBool,
    generation: AtomicU64,
    /// Wakes the hub immediately for a resync (context change).
    wake: tokio::sync::Notify,
}

impl Hub {
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn counts(&self) -> (usize, usize) {
        (self.subscribers.load(Ordering::Acquire), self.out.receiver_count())
    }
}

#[derive(Default)]
pub struct LiveHubs {
    map: Mutex<HashMap<HubKey, Arc<Hub>>>,
    next_session: AtomicU64,
    next_generation: AtomicU64,
}

/// Keeps a viewer registered; dropping it unsubscribes.
pub struct Subscription {
    pub rx: broadcast::Receiver<Arc<str>>,
    pub hub: Arc<Hub>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.hub.subscribers.fetch_sub(1, Ordering::AcqRel);
    }
}

impl LiveHubs {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            next_session: AtomicU64::new(1),
            // Generations are unique across hub lifetimes within a process and
            // across restarts (seeded from wall clock).
            next_generation: AtomicU64::new(chrono::Utc::now().timestamp_millis().max(1) as u64),
        }
    }

    fn generation(&self) -> u64 {
        self.next_generation.fetch_add(1, Ordering::AcqRel)
    }

    /// Non-blocking hand-off from ingestion. Never awaits.
    /// `rev` is the source input revision also sent on the base bar frame.
    pub fn notify(&self, key: HubKey, bar: &Bar, rev: u64) {
        let hub = self.map.lock().unwrap().get(&key).cloned();
        if let Some(hub) = hub {
            if hub.events.try_send((bar.clone(), rev)).is_err() {
                hub.resync.store(true, Ordering::Release);
            }
        }
    }

    /// Atomic stop decision, under the same lock `subscribe` registers with:
    /// returns true (and unmaps the hub if still mapped) iff it has no
    /// subscribers. A concurrent joiner either was counted first (hub stays)
    /// or finds the key unmapped and creates a fresh hub.
    pub(crate) fn try_retire(&self, key: HubKey, hub: &Arc<Hub>) -> bool {
        let mut map = self.map.lock().unwrap();
        if hub.subscribers.load(Ordering::Acquire) != 0 {
            return false;
        }
        if map.get(&key).is_some_and(|h| Arc::ptr_eq(h, hub)) {
            map.remove(&key);
        }
        true
    }

    pub fn active(&self) -> usize {
        self.map.lock().unwrap().len()
    }

    /// Cached context of a scope changed outside the live delta path (history
    /// warm-up/repair). A running hub resets (new generation) and replays its
    /// bounded window from the new cache promptly. No-op without a hub.
    pub fn context_changed(&self, key: HubKey) {
        let hub = self.map.lock().unwrap().get(&key).cloned();
        if let Some(hub) = hub {
            hub.resync.store(true, Ordering::Release);
            hub.wake.notify_one();
        }
    }

    /// Subscribe a viewer; starts the hub on first subscriber. Returns `None`
    /// when no indicators are configured.
    pub fn subscribe(self: &Arc<Self>, state: &Arc<AppState>, key: HubKey) -> Option<Subscription> {
        if state.indicators.ids().is_empty() {
            return None;
        }
        // Register the viewer (count + receiver) under the map lock BEFORE the
        // producer can run, so a new hub's bootstrap/status frames always have
        // a receiver and the hub never observes a transient zero count.
        // Retirement (`try_retire`) takes the same lock, so a joining viewer
        // either joins a live hub or creates a fresh one, never a retiring one.
        let mut map = self.map.lock().unwrap();
        if let Some(h) = map.get(&key) {
            let hub = h.clone();
            hub.subscribers.fetch_add(1, Ordering::AcqRel);
            let rx = hub.out.subscribe();
            return Some(Subscription { rx, hub });
        }
        let (tx, rx_ev) = mpsc::channel(HUB_QUEUE);
        let (out, rx) = broadcast::channel(HUB_FANOUT);
        let hub = Arc::new(Hub {
            events: tx,
            out,
            subscribers: AtomicUsize::new(1),
            resync: AtomicBool::new(false),
            generation: AtomicU64::new(self.generation()),
            wake: tokio::sync::Notify::new(),
        });
        map.insert(key, hub.clone());
        drop(map);
        tokio::spawn(run_hub(self.clone(), state.clone(), key, hub.clone(), rx_ev));
        Some(Subscription { rx, hub })
    }
}

struct Session {
    id: String,
    mode: Mode,
    session: u64,
    seq: u64,
    open: bool,
    /// Live window bounds known to the runner session.
    first_time: i64,
    last_time: i64,
    version: String,
    epoch: u64,
    /// Fail-closed: the required input window contains invalid canonical input.
    poisoned: bool,
    /// Newest base revision of THIS tf reflected in the session's input
    /// (snapshot rev at open, then max rev of applied events). 0 = no live
    /// base frame of this tf covered yet.
    as_of_rev: u64,
}

/// Indicator WebSocket frames (sent separately from base bar frames).
///
/// Ordering/fencing key for clients: connection scope + `generation` +
/// (`id`, `mode`, `session`) + `seq`. A runner restart opens a new `session`
/// starting again at `seq` 0 without necessarily changing `generation`; an
/// older session or lower seq must never overwrite a newer one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IndicatorFrame {
    /// Columnar output for bars from the first touched bar to the newest.
    /// `as_of_rev` is exactly the `rev` of the newest base frame OF THIS
    /// SCOPE (exchange, market, symbol, tf) whose bar version is included in
    /// the input, i.e. the input equals the scope state after that frame
    /// (0 = no live base frame of this tf covered, history-only input). It is
    /// never a revision belonging only to another tf. A client may treat the
    /// output as current only once it has observed the base frame with that
    /// rev (or a newer one) for the scope and, per bar, `as_of_rev` >= the rev
    /// of the base frame shown for that bar.
    Indicator {
        api_version: u32,
        id: String,
        version: String,
        mode: String,
        generation: u64,
        session: u64,
        seq: u64,
        as_of_rev: u64,
        output: Output,
    },
    /// Session opened (`ok`) or became unavailable (error is public only).
    IndicatorStatus {
        api_version: u32,
        id: String,
        mode: String,
        generation: u64,
        session: u64,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<PluginError>,
    },
    /// Hub resynchronised or this viewer lagged: refetch history, drop live
    /// indicator state from older generations.
    IndicatorReset { api_version: u32, generation: u64 },
}

impl IndicatorFrame {
    pub fn reset(generation: u64) -> Self {
        Self::IndicatorReset { api_version: crate::indicators::API_VERSION, generation }
    }
    pub fn to_text(&self) -> Arc<str> {
        Arc::from(serde_json::to_string(self).expect("frame serialises"))
    }
}

fn emit(hub: &Hub, f: IndicatorFrame) {
    let _ = hub.out.send(f.to_text());
}

fn status_msg(hub: &Hub, s: &Session, err: Option<&PluginError>) {
    emit(
        hub,
        IndicatorFrame::IndicatorStatus {
            api_version: crate::indicators::API_VERSION,
            id: s.id.clone(),
            mode: mode_str(s.mode).into(),
            generation: hub.generation(),
            session: s.session,
            status: if err.is_some() { "unavailable" } else { "ok" }.into(),
            error: err.cloned(),
        },
    );
}

fn output_msg(hub: &Hub, s: &Session, as_of_rev: u64, output: Output) {
    emit(
        hub,
        IndicatorFrame::Indicator {
            api_version: crate::indicators::API_VERSION,
            id: s.id.clone(),
            version: s.version.clone(),
            mode: mode_str(s.mode).into(),
            generation: hub.generation(),
            session: s.session,
            seq: s.seq,
            as_of_rev,
            output,
        },
    );
}

/// Coalesced deltas: latest version (and its rev) of each bucket.
#[derive(Default)]
struct Batch {
    bars: BTreeMap<i64, (Bar, u64)>,
}

/// Coherent snapshot of cached closed bars plus forming bar (aggregates
/// already applied) together with the rev of the newest base frame of this tf
/// it reflects. Taken under the tf publication lock, so content and rev match
/// and no event with rev <= the returned rev is missing from the content.
pub(crate) async fn snapshot(state: &AppState, key: HubKey, max_bars: usize) -> (Vec<Bar>, u64) {
    let (e, m, s, tf) = key;
    let Some(shard) = state.shards.get(&(e, m, s)) else { return (Vec::new(), 0) };
    let published = shard.published_for(tf).lock().await;
    let (hist, _) = shard.for_tf(tf);
    let mut bars: Vec<Bar> = {
        let h = hist.read().await;
        h.iter().skip(h.len().saturating_sub(max_bars)).cloned().collect()
    };
    if let Some(live) = shard.live_bar_for_tf(tf).read().await.clone() {
        match bars.last() {
            Some(last) if last.time == live.time => *bars.last_mut().unwrap() = live,
            Some(last) if last.time > live.time => {}
            _ => bars.push(live),
        }
    }
    bars.retain(Bar::is_renderable);
    (bars, *published)
}

/// Base bootstrap at hub start / resync, before any session snapshot: re-send
/// the newest published bar of the tf as an ordinary base frame with its own
/// rev R0 (nothing if no frame of this tf was published yet). Viewers are
/// subscribed to base frames before the hub starts, so they observe R0 (or a
/// newer live frame, which is >= every subsequent snapshot rev) before any
/// initial result; WebSocket delivery is base-first (biased select).
async fn bootstrap_base(state: &AppState, key: HubKey) {
    let (e, m, s, tf) = key;
    let Some(shard) = state.shards.get(&(e, m, s)) else { return };
    let published = shard.published_for(tf).lock().await;
    if *published == 0 {
        return;
    }
    let tail = match shard.live_bar_for_tf(tf).read().await.clone() {
        Some(live) => Some(live),
        None => shard.for_tf(tf).0.read().await.back().cloned(),
    };
    let Some(tail) = tail.filter(Bar::is_renderable) else { return };
    let _ = shard.for_tf(tf).1.send(crate::models::WsPayload {
        rev: *published,
        series: crate::series_for_market(m),
        action: crate::models::WsAction::ReplaceLast,
        bar: tail,
    });
}

/// True while an invalid (dropped) bar lies inside the live window of the
/// newest `ctx + 1` clean bars (or the cache is shorter than that). Clears only
/// once enough newer clean bars push it out, or a renderable version of the
/// same bucket replaces it; never on an unrelated good tick alone.
async fn input_poisoned(state: &AppState, key: HubKey, ctx: usize) -> bool {
    let (e, m, s, tf) = key;
    let Some(shard) = state.shards.get(&(e, m, s)) else { return false };
    let invalid = shard.invalid_times_for(tf);
    if invalid.is_empty() {
        return false;
    }
    let (bars, _) = snapshot(state, key, ctx + 1).await;
    let start = (bars.len() > ctx).then(|| bars[0].time);
    crate::indicators::window_poisoned(&invalid, start)
}

/// Returns true while the session must stay closed because of invalid input;
/// emits `unavailable` once on the transition.
async fn hold_if_poisoned(
    mgr: &IndicatorManager,
    state: &AppState,
    key: HubKey,
    hub: &Hub,
    s: &mut Session,
    ctx: usize,
) -> bool {
    if !input_poisoned(state, key, ctx).await {
        s.poisoned = false;
        return false;
    }
    if !s.poisoned {
        if s.open {
            mgr.live_close(&s.id, s.session);
        }
        s.open = false;
        s.poisoned = true;
        status_msg(hub, s, Some(&crate::indicators::invalid_input_window()));
    }
    true
}

fn scope(key: HubKey, mode: Mode) -> Scope {
    Scope {
        exchange: key.0.as_str().into(),
        market: key.1.as_str().into(),
        symbol: key.2.as_str().into(),
        tf: key.3.as_str().into(),
        mode,
    }
}

async fn open_session(
    hubs: &LiveHubs,
    mgr: &IndicatorManager,
    state: &AppState,
    key: HubKey,
    hub: &Hub,
    s: &mut Session,
) {
    let Some((desc, epoch)) = mgr.ready(&s.id) else {
        s.open = false;
        status_msg(hub, s, Some(&crate::indicators::unavailable("indicator not ready")));
        return;
    };
    let ctx = desc.context.bars_for(key.3.as_str()) as usize;
    if hold_if_poisoned(mgr, state, key, hub, s, ctx).await {
        return;
    }
    if s.open {
        mgr.live_close(&s.id, s.session);
    }
    // Content and rev from one coherent per-tf snapshot.
    let (bars, as_of_rev) = snapshot(state, key, ctx + 1).await;
    s.session = hubs.next_session.fetch_add(1, Ordering::AcqRel);
    s.seq = 0;
    s.as_of_rev = as_of_rev;
    s.version = desc.version.clone();
    s.epoch = epoch;
    let init =
        LiveInit { scope: scope(key, s.mode), revision: hub.generation(), bars: bars.iter().map(input_bar).collect() };
    match mgr.live_open(&s.id, s.session, hub.generation(), init).await {
        Ok(out) => {
            s.open = true;
            s.first_time = bars.first().map(|b| b.time).unwrap_or(0);
            s.last_time = bars.last().map(|b| b.time).unwrap_or(0);
            status_msg(hub, s, None);
            if !out.times.is_empty() {
                output_msg(hub, s, as_of_rev, out);
            }
        }
        Err(e) => {
            s.open = false;
            status_msg(hub, s, Some(&e));
        }
    }
}

async fn run_hub(
    hubs: Arc<LiveHubs>,
    state: Arc<AppState>,
    key: HubKey,
    hub: Arc<Hub>,
    mut rx: mpsc::Receiver<(Bar, u64)>,
) {
    let mgr = state.indicators.clone();
    let mut sessions: Vec<Session> = Vec::new();
    for id in mgr.ids() {
        for mode in mgr.modes(&id) {
            sessions.push(Session {
                id: id.clone(),
                mode,
                session: 0,
                seq: 0,
                open: false,
                first_time: 0,
                last_time: 0,
                version: String::new(),
                epoch: 0,
                poisoned: false,
                as_of_rev: 0,
            });
        }
    }
    open_all(&hubs, &mgr, &state, key, &hub, &mut sessions).await;
    let mut tick = tokio::time::interval(HUB_TICK);
    loop {
        let first = tokio::select! {
            ev = rx.recv() => ev,
            _ = hub.wake.notified() => None,
            _ = tick.tick() => None,
        };
        if hub.subscribers.load(Ordering::Acquire) == 0 && hubs.try_retire(key, &hub) {
            break;
        }
        if hub.resync.swap(false, Ordering::AcqRel) {
            hub.generation.store(hubs.generation(), Ordering::Release);
            emit(&hub, IndicatorFrame::reset(hub.generation()));
            while rx.try_recv().is_ok() {}
            open_all(&hubs, &mgr, &state, key, &hub, &mut sessions).await;
            continue;
        }
        // Coalesce queued bars: keep the latest version of each bucket.
        let mut batch = Batch::default();
        // Revs arrive in publication order, so the last insert per bucket is
        // its newest version.
        for (b, rev) in first.into_iter().chain(std::iter::from_fn(|| rx.try_recv().ok())) {
            batch.bars.insert(b.time, (b, rev));
        }
        // Sessions advance concurrently: one slow indicator cannot delay others.
        let batch = &batch;
        let (hubs_r, mgr_r, state_r, hub_r) = (&*hubs, &*mgr, &*state, &*hub);
        futures::future::join_all(
            sessions.iter_mut().map(|s| step_session(hubs_r, mgr_r, state_r, key, hub_r, s, batch)),
        )
        .await;
    }
    for s in &sessions {
        if s.open {
            mgr.live_close(&s.id, s.session);
        }
    }
}

async fn step_session(
    hubs: &LiveHubs,
    mgr: &IndicatorManager,
    state: &AppState,
    key: HubKey,
    hub: &Hub,
    s: &mut Session,
    batch: &Batch,
) {
    // Re-open closed sessions or sessions from a previous runner epoch by
    // replaying the bounded window from cache.
    let ready = mgr.ready(&s.id);
    let stale_epoch = ready.as_ref().is_some_and(|(_, epoch)| *epoch != s.epoch);
    if let Some((desc, _)) = &ready {
        let ctx = desc.context.bars_for(key.3.as_str()) as usize;
        let was = s.poisoned;
        if hold_if_poisoned(mgr, state, key, hub, s, ctx).await {
            return;
        }
        if was {
            // Validated reset: reopen from a clean window.
            open_session(hubs, mgr, state, key, hub, s).await;
            return;
        }
    }
    if !s.open || stale_epoch {
        if ready.is_some() {
            open_session(hubs, mgr, state, key, hub, s).await;
        }
        return; // the snapshot already contains this batch
    }
    if batch.bars.is_empty() {
        return;
    }
    let mut ops = Vec::new();
    let mut last = s.last_time;
    let mut as_of_rev = s.as_of_rev;
    for (b, rev) in batch.bars.values() {
        // Already contained in the session's snapshot (queued before open).
        if *rev <= s.as_of_rev {
            continue;
        }
        as_of_rev = as_of_rev.max(*rev);
        if b.time > last {
            ops.push(LiveOp::Append { bar: input_bar(b) });
            last = b.time;
        } else if b.time >= s.first_time {
            ops.push(LiveOp::Replace { bar: input_bar(b) });
        }
    }
    if ops.is_empty() {
        return;
    }
    s.seq += 1;
    let upd = LiveUpdate { revision: hub.generation(), seq: s.seq, ops };
    match mgr.live_update(&s.id, s.session, hub.generation(), upd).await {
        Ok(out) => {
            s.last_time = last;
            s.as_of_rev = as_of_rev;
            output_msg(hub, s, as_of_rev, out);
        }
        Err(e) => {
            s.open = false;
            status_msg(hub, s, Some(&e));
        }
    }
}

async fn open_all(
    hubs: &LiveHubs,
    mgr: &IndicatorManager,
    state: &AppState,
    key: HubKey,
    hub: &Hub,
    sessions: &mut [Session],
) {
    bootstrap_base(state, key).await;
    futures::future::join_all(sessions.iter_mut().map(|s| open_session(hubs, mgr, state, key, hub, s))).await;
}

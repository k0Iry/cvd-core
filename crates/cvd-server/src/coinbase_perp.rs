#[path = "drb.rs"]
pub(crate) mod drb;
#[cfg(test)]
#[path = "drb_tests.rs"]
mod drb_tests;

use crate::{
    types::{Exchange, Market, Symbol},
    AppState,
};
use anyhow::{anyhow, Result};
use futures::{SinkExt, StreamExt};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_tungstenite::{tungstenite::Message, MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

pub async fn run_coinbase_perp_market_engine(
    state: Arc<AppState>,
    write: futures::stream::SplitSink<Socket, Message>,
    read: futures::stream::SplitStream<Socket>,
) -> Result<()> {
    run_with_base(state, write, read, drb::BASE.to_string()).await
}

// Inject the public HTTP base for loopback tests; the production entry keeps the same signature.
pub(crate) async fn run_with_base(
    state: Arc<AppState>,
    write: futures::stream::SplitSink<Socket, Message>,
    read: futures::stream::SplitStream<Socket>,
    base: String,
) -> Result<()> {
    run_with_base_until(state, write, read, base, std::future::pending::<()>()).await
}

pub(crate) async fn run_with_base_until<F: std::future::Future<Output = ()>>(
    state: Arc<AppState>,
    mut write: futures::stream::SplitSink<Socket, Message>,
    mut read: futures::stream::SplitStream<Socket>,
    base: String,
    shutdown: F,
) -> Result<()> {
    // Check both independently. No symbol may be admitted before its contract
    // size has been validated, but a transient failure must not strand its peer.
    let (btc, eth) =
        tokio::join!(drb::metadata(&state.http, &base, Symbol::Btc), drb::metadata(&state.http, &base, Symbol::Eth),);
    let mut sizes = HashMap::new();
    let mut missing = Vec::new();
    for (symbol, result) in [(Symbol::Btc, btc), (Symbol::Eth, eth)] {
        match result {
            Ok(size) => {
                sizes.insert(symbol, size);
            }
            Err(drb::HistoryError::Retry(e)) => {
                eprintln!("DRB metadata temporarily unavailable for {}: {e}", drb::product(symbol));
                missing.push(symbol);
            }
            Err(drb::HistoryError::Fatal(e)) => {
                crate::recovery::reject_source(
                    &state,
                    Exchange::Coinbase,
                    Market::Usdc,
                    symbol,
                    format!("invalid DRB instrument: {e}"),
                )
                .await;
            }
        }
    }
    anyhow::ensure!(!sizes.is_empty(), "no validated DRB instruments; retry connection");
    let mut senders = HashMap::new();
    let mut receivers = HashMap::new();
    let mut workers = tokio::task::JoinSet::new();
    for symbol in [Symbol::Btc, Symbol::Eth] {
        if sizes.contains_key(&symbol) || missing.contains(&symbol) {
            let (tx, rx) = mpsc::channel(crate::recovery::RECOVERY_CHANNEL_CAPACITY);
            senders.insert(symbol, tx);
            receivers.insert(symbol, rx);
        }
    }
    for (&symbol, &size) in &sizes {
        workers.spawn(drb::worker(state.clone(), symbol, receivers.remove(&symbol).unwrap(), base.clone(), size));
    }
    let (validated_tx, mut validated_rx) = mpsc::channel::<(Symbol, f64)>(2);
    // Independent bounded tail observer: it never blocks the socket/heartbeat reader.
    // Its only output is the same per-symbol FIFO input as WebSocket executions.
    let (stop_tail, mut stopped) = tokio::sync::oneshot::channel::<()>();
    let mut tail_workers = tokio::task::JoinSet::new();
    {
        let state = state.clone();
        let base = base.clone();
        let mut tail_sizes = sizes.clone();
        let mut missing = missing;
        let txs = senders.clone();
        tail_workers.spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(15));
            ticker.tick().await;
            'tail: loop {
                tokio::select! { _=&mut stopped=>break 'tail, _=ticker.tick()=>{} }
                for (&symbol, &size) in &tail_sizes {
                    if crate::recovery::is_terminal(&state, Exchange::Coinbase, Market::Usdc, symbol).await {
                        continue;
                    }
                    let request = drb::latest(&state.http, &base, symbol, size);
                    tokio::pin!(request);
                    let latest = tokio::select! { _=&mut stopped=>break 'tail, result=&mut request=>result };
                    match latest {
                        Ok(Some(trade)) => {
                            if let Some(tx) = txs.get(&symbol) {
                                crate::recovery::enqueue_trade(
                                    &state,
                                    Exchange::Coinbase,
                                    Market::Usdc,
                                    symbol,
                                    tx,
                                    trade,
                                )
                                .await;
                            }
                        }
                        Ok(None) => {}
                        Err(drb::HistoryError::Retry(error)) => {
                            eprintln!("DRB latest temporarily unavailable for {symbol:?}; retry next poll: {error}");
                        }
                        Err(drb::HistoryError::Fatal(error)) => {
                            crate::recovery::reject_source(
                                &state,
                                Exchange::Coinbase,
                                Market::Usdc,
                                symbol,
                                format!("DRB latest check failed: {error}"),
                            )
                            .await
                        }
                    }
                }
                for symbol in std::mem::take(&mut missing) {
                    if crate::recovery::is_terminal(&state, Exchange::Coinbase, Market::Usdc, symbol).await {
                        continue;
                    }
                    let request = drb::metadata(&state.http, &base, symbol);
                    tokio::pin!(request);
                    let outcome = tokio::select! {
                        _=&mut stopped=>break 'tail,
                        result=&mut request=>result,
                    };
                    match outcome {
                        Ok(size) => {
                            tail_sizes.insert(symbol, size);
                            // The socket reader owns receiver startup and cannot see a
                            // contract size until this bounded handoff completes.
                            if validated_tx.send((symbol, size)).await.is_err() {
                                break 'tail;
                            }
                        }
                        Err(drb::HistoryError::Retry(error)) => {
                            eprintln!("DRB metadata still unavailable for {symbol:?}: {error}");
                            missing.push(symbol);
                        }
                        Err(drb::HistoryError::Fatal(error)) => {
                            crate::recovery::reject_source(
                                &state,
                                Exchange::Coinbase,
                                Market::Usdc,
                                symbol,
                                format!("invalid DRB instrument: {error}"),
                            )
                            .await;
                        }
                    }
                }
            }
        });
    }
    let result: Result<()> = async {
        tokio::pin!(shutdown);
        write.send(drb::rpc(1,"public/subscribe",serde_json::json!({"channels":drb::channels()}))).await?;
        write.send(drb::rpc(2,"public/set_heartbeat",serde_json::json!({"interval":10}))).await?;
        let mut subscribe=false;
        let mut heartbeat=false;
        let mut next_id=3_u64;
        let ack_deadline=tokio::time::Instant::now()+Duration::from_secs(10);
        loop {
            tokio::select! {
                _=&mut shutdown=>break,
                Some((symbol, size))=validated_rx.recv()=>{
                    if !crate::recovery::is_terminal(&state, Exchange::Coinbase, Market::Usdc, symbol).await {
                        if let Some(rx) = receivers.remove(&symbol) {
                            sizes.insert(symbol, size);
                            workers.spawn(drb::worker(state.clone(), symbol, rx, base.clone(), size));
                        }
                    }
                },
                _=tokio::time::sleep_until(ack_deadline), if !subscribe || !heartbeat=>return Err(anyhow!("DRB subscription/heartbeat acknowledgement timed out")),
                message=read.next()=> {
                    let msg=match message {Some(Ok(m))=>m,Some(Err(e))=>return Err(e.into()),None=>break};
                    match msg {
                        Message::Ping(payload)=>write.send(Message::Pong(payload)).await?,
                        Message::Close(_)=>break,
                        Message::Text(text)=>{
                            let v:serde_json::Value=serde_json::from_str(&text)?;
                            if let Some(err)=v.get("error") {return Err(anyhow!("DRB RPC error: {err}"));}
                            if let Some(id)=v.get("id") {
                                let id=id.as_u64().ok_or_else(||anyhow!("invalid DRB RPC response id"))?;
                                match id {
                                    1=>{
                                        let channels=v["result"].as_array().ok_or_else(||anyhow!("invalid DRB subscribe reply"))?;
                                        let expected=drb::channels();
                                        anyhow::ensure!(channels.len()==expected.len() && expected.iter().all(|c|channels.iter().any(|v|v.as_str()==Some(c))),"DRB subscribe acknowledgement missing channel");
                                        subscribe=true;
                                    },
                                    2=>{anyhow::ensure!(v["result"]=="ok","invalid DRB heartbeat reply");heartbeat=true;},
                                    n if n>=3 && n<next_id=>{anyhow::ensure!(v.get("result").is_some(),"invalid heartbeat test response");},
                                    _=>return Err(anyhow!("unexpected DRB RPC response id {id}")),
                                }
                                continue;
                            }
                            if v["method"]=="heartbeat" {
                                if v["params"]["type"]=="test_request" {
                                    write.send(drb::rpc(next_id,"public/test",serde_json::json!({}))).await?;
                                    next_id=next_id.checked_add(1).ok_or_else(||anyhow!("RPC id overflow"))?;
                                }
                                continue;
                            }
                            if v["method"]=="subscription" {
                                anyhow::ensure!(subscribe,"DRB trade before subscription acknowledgement");
                                let channel=v["params"]["channel"].as_str().unwrap_or("");
                                let pending_symbol = [Symbol::Btc, Symbol::Eth].into_iter().find(|&s| {
                                    channel == format!("trades.{}.100ms", drb::product(s)) && receivers.contains_key(&s)
                                });
                                // Never parse or queue a trade without validated contract metadata.
                                // An existing durable cursor will recover from the first later
                                // validated observation; without one, this starts a new segment.
                                if pending_symbol.is_some() { continue; }
                                match drb::decode_notification(&v,&sizes) {
                                    Ok(Some((symbol,trades)))=>for trade in trades {
                                        if let Some(tx)=senders.get(&symbol) {crate::recovery::enqueue_trade(&state,Exchange::Coinbase,Market::Usdc,symbol,tx,trade).await;}
                                    },
                                    Err(error)=>{
                                        let channel=v["params"]["channel"].as_str().unwrap_or("");
                                        let symbol=if channel.contains("BTC_USDC-PERPETUAL") {Some(Symbol::Btc)} else if channel.contains("ETH_USDC-PERPETUAL") {Some(Symbol::Eth)} else {None};
                                        if let Some(symbol)=symbol {crate::recovery::reject_source(&state,Exchange::Coinbase,Market::Usdc,symbol,format!("malformed subscribed DRB trade: {error}")).await;}
                                        else {return Err(anyhow!("malformed DRB subscription: {error}"));}
                                    },
                                    _=>{},
                                }
                            }
                        },
                        _=>{},
                    }
                }
            }
        }
        Err(anyhow!("DRB websocket ended"))
    }.await;
    let _ = stop_tail.send(());
    // Join every producer before closing FIFO senders. A panicking tail must not
    // abort an ingestion worker inside a non-cancellation-safe flush.
    let tail_result = crate::recovery::finish_workers(tail_workers).await;
    drop(senders);
    drop(receivers);
    let worker_result = crate::recovery::finish_workers(workers).await;
    tail_result?;
    worker_result?;
    result
}

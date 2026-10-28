use anyhow::{anyhow, Result};
use futures::{SinkExt, StreamExt};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_tungstenite::tungstenite::Message;

use crate::ingestion::Trade;
use crate::models::{CoinbaseHeartbeat, CoinbaseMatch, CoinbaseSubscribe};
use crate::types::{Exchange, Market, Symbol};
use crate::AppState;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;

fn spot_symbols() -> &'static [Symbol] {
    &[Symbol::Btc, Symbol::Eth]
}

pub async fn run_coinbase_spot_market_engine(
    state: Arc<AppState>,
    write: futures::stream::SplitSink<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, Message>,
    read: futures::stream::SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>,
) -> Result<()> {
    run_coinbase_spot_market_engine_with_base(state, write, read, "https://api.exchange.coinbase.com".into()).await
}

pub(crate) async fn run_coinbase_spot_market_engine_with_base(
    state: Arc<AppState>,
    mut write: futures::stream::SplitSink<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, Message>,
    mut read: futures::stream::SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>,
    spot_base: String,
) -> Result<()> {
    let exchange = Exchange::Coinbase;
    let market = Market::Spot;
    let product_ids = spot_symbols().iter().filter_map(|symbol| symbol.coinbase_spot_product()).collect::<Vec<_>>();

    let sub = CoinbaseSubscribe { msg_type: "subscribe", product_ids, channels: vec!["matches", "heartbeat"] };
    write.send(Message::Text(serde_json::to_string(&sub)?.into())).await?;

    let mut ping = tokio::time::interval(Duration::from_secs(20));
    let mut senders = HashMap::new();
    let mut workers = tokio::task::JoinSet::new();
    for &symbol in spot_symbols() {
        let (tx, rx) = mpsc::channel(crate::recovery::RECOVERY_CHANNEL_CAPACITY);
        senders.insert(symbol, tx);
        workers.spawn(crate::recovery::spot::run_spot_worker_with_base(state.clone(), symbol, rx, spot_base.clone()));
    }
    let result: Result<()> = async {
    loop {
        tokio::select! {
            _ = ping.tick() => {
                write.send(Message::Ping(vec![].into())).await?;
            }

            msg = read.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(e.into()),
                    None => break,
                };

                match msg {
                    Message::Ping(payload) => {
                        write.send(Message::Pong(payload)).await?;
                    }
                    Message::Text(txt) => {
                        // A malformed subscribed data message is not harmless: otherwise
                        // an unparseable match/heartbeat can hide a missing execution.
                        let routing: serde_json::Value = match serde_json::from_str(&txt) {
                            Ok(value) => value,
                            Err(_) => continue, // no product can be identified
                        };
                        let routed_symbol = routing.get("product_id").and_then(|v| v.as_str())
                            .and_then(Symbol::from_coinbase_spot_product);
                        let kind = routing.get("type").and_then(|v| v.as_str());
                        if let Ok(heartbeat) = serde_json::from_str::<CoinbaseHeartbeat>(&txt) {
                            if heartbeat.msg_type == "heartbeat" {
                                if let Some(symbol) = Symbol::from_coinbase_spot_product(&heartbeat.product_id) {
                                    if let Some(sender) = senders.get(&symbol) {
                                        crate::recovery::enqueue_spot(&state, symbol, sender, crate::recovery::SpotInput::Heartbeat(heartbeat.last_trade_id)).await;
                                    }
                                }
                                continue;
                            }
                        }
                        if kind == Some("heartbeat") {
                            if let Some(symbol) = routed_symbol {
                                crate::recovery::reject_source(&state, exchange, market, symbol, "malformed Coinbase heartbeat; source paused".into()).await;
                            }
                            continue;
                        }
                        let m: CoinbaseMatch = match serde_json::from_str(&txt) {
                            Ok(v) => v,
                            Err(_) => {
                                if kind == Some("match") {
                                    if let Some(symbol) = routed_symbol {
                                        crate::recovery::reject_source(&state, exchange, market, symbol, "malformed Coinbase match; source paused".into()).await;
                                    }
                                }
                                continue;
                            },
                        };
                        if m.msg_type != "match" {
                            continue;
                        }

                        let Some(symbol) = Symbol::from_coinbase_spot_product(&m.product_id) else {
                            continue;
                        };
                        let price: f64 = match m.price.parse() {
                            Ok(v) => v,
                            Err(_) => { crate::recovery::reject_source(&state, exchange, market, symbol, "invalid trade price/size; source paused".into()).await; continue; },
                        };
                        let size: f64 = match m.size.parse() {
                            Ok(v) => v,
                            Err(_) => { crate::recovery::reject_source(&state, exchange, market, symbol, "invalid trade price/size; source paused".into()).await; continue; },
                        };

                        if !matches!(m.side.as_str(), "buy" | "sell") {
                            crate::recovery::reject_source(&state, exchange, market, symbol, "invalid Coinbase trade side".into()).await;
                            continue;
                        }
                        let timestamp_ms = match crate::parse_iso8601_to_ms(&m.time) {
                            Ok(value) => value,
                            Err(error) => {
                                crate::recovery::reject_source(&state, exchange, market, symbol, format!("invalid Coinbase trade timestamp: {error}")).await;
                                continue;
                            }
                        };
                        let trade = Trade {
                            id: m.trade_id.to_string(), timestamp_ms,
                            price, base: size, quote: size * price, is_buy: m.side == "sell",
                        };
                        if let Some(sender) = senders.get(&symbol) {
                            crate::recovery::enqueue_spot(&state, symbol, sender, crate::recovery::SpotInput::Trade(trade)).await;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }

    Err(anyhow!("ws ended"))
    }.await;
    // Never abort a worker inside the non-cancellation-safe bar flush.
    drop(senders);
    crate::recovery::finish_workers(workers).await?;
    result
}

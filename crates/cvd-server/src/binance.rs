use anyhow::{anyhow, Result};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::HashMap;
use tokio::time::{self, Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

use crate::ingestion::{ingest_trade, Trade};
use crate::models::{AggTrade, BinanceCombinedMsg};
use crate::types::{Exchange, Market, Symbol};
use crate::AppState;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;

fn market_symbols(market: Market) -> &'static [Symbol] {
    match market {
        Market::Spot | Market::Usdm | Market::Coinm => &[Symbol::Btc, Symbol::Eth],
        Market::Usdc => panic!("Binance does not support the Coinbase USDC gateway"),
    }
}

#[derive(Deserialize)]
struct ExchangeInfo {
    symbols: Vec<ContractInfo>,
}

#[derive(Deserialize)]
struct ContractInfo {
    symbol: String,
    #[serde(rename = "contractSize")]
    contract_size: f64,
}

async fn load_contract_sizes() -> Result<HashMap<Symbol, f64>> {
    let info: ExchangeInfo = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?
        .get("https://dapi.binance.com/dapi/v1/exchangeInfo")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    contract_sizes_from_info(info)
}

fn contract_sizes_from_info(info: ExchangeInfo) -> Result<HashMap<Symbol, f64>> {
    market_symbols(Market::Coinm)
        .iter()
        .map(|&symbol| {
            let contract = info
                .symbols
                .iter()
                .find(|contract| contract.symbol.eq_ignore_ascii_case(symbol.coinm_stream()))
                .ok_or_else(|| anyhow!("missing contractSize for {}", symbol.coinm_stream()))?;
            anyhow::ensure!(contract.contract_size.is_finite() && contract.contract_size > 0.0, "invalid contractSize");
            Ok((symbol, contract.contract_size))
        })
        .collect()
}

pub(crate) fn trade_volumes(
    market: Market,
    price: f64,
    quantity: f64,
    contract_size: Option<f64>,
) -> Result<(f64, f64)> {
    anyhow::ensure!(
        price.is_finite() && price > 0.0 && quantity.is_finite() && quantity > 0.0,
        "invalid trade price or quantity"
    );
    let (base, quote) = if market == Market::Coinm {
        let size = contract_size.ok_or_else(|| anyhow!("COIN-M contractSize unavailable"))?;
        anyhow::ensure!(size.is_finite() && size > 0.0, "invalid contractSize");
        let quote = quantity * size;
        (quote / price, quote)
    } else {
        (quantity, quantity * price)
    };
    anyhow::ensure!(base.is_finite() && quote.is_finite(), "trade volume overflow");
    Ok((base, quote))
}

pub async fn run_binance_market_engine(
    state: &AppState,
    market: Market,
    mut write: futures::stream::SplitSink<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, Message>,
    mut read: futures::stream::SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>,
) -> Result<()> {
    let exchange = Exchange::Binance;
    let mut keepalive = time::interval(Duration::from_secs(20));
    let mut watchdog = time::interval(Duration::from_secs(10));
    let mut last_message_at = Instant::now();
    let contract_sizes = if market == Market::Coinm { load_contract_sizes().await? } else { HashMap::new() };

    loop {
        tokio::select! {
            _ = keepalive.tick() => {
                // Spot docs require ponging server pings; unsolicited pong is also allowed.
                write.send(Message::Pong(vec![].into())).await?;
            }

            _ = watchdog.tick() => {
                if last_message_at.elapsed() > Duration::from_secs(60) {
                    return Err(anyhow!(
                        "[{}:{}] ws inactivity timeout after {:?}",
                        exchange.as_str(),
                        market.as_str(),
                        last_message_at.elapsed()
                    ));
                }
            }

            msg = read.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => {
                        eprintln!("[{}:{}] read err: {e}", exchange.as_str(), market.as_str());
                        break;
                    }
                    None => break,
                };
                last_message_at = Instant::now();

                match msg {
                    Message::Ping(payload) => {
                        write.send(Message::Pong(payload)).await?;
                    }
                    Message::Text(txt) => {
                        let combined: BinanceCombinedMsg = match serde_json::from_str(&txt) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };

                        let symbol = match Symbol::from_binance_stream_name(&combined.stream, market) {
                            Some(symbol) => symbol,
                            None => continue,
                        };

                        let t: AggTrade = match serde_json::from_value(combined.data) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if t.event_type != "aggTrade" {
                            continue;
                        }

                        let price: f64 = match t.price.parse() {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let qty: f64 = match t.quantity.parse() {
                            Ok(v) => v,
                            Err(_) => continue,
                        };

                        let (base, quote) = trade_volumes(market, price, qty, contract_sizes.get(&symbol).copied())?;
                        let trade = Trade {
                            id: t.id.to_string(), timestamp_ms: t.trade_time,
                            price, base, quote, is_buy: !t.is_buyer_maker,
                        };
                        crate::recovery::binance_before(state, market, symbol, &trade, contract_sizes.get(&symbol).copied()).await?;
                        ingest_trade(state, exchange, market, symbol, trade).await?;
                        crate::recovery::mark_live(state, exchange, market, symbol).await;
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }

    Err(anyhow!("ws ended"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverse_contracts_use_face_value_and_base_conversion() {
        let info = serde_json::from_str(
            r#"{"symbols":[{"symbol":"BTCUSD_PERP","contractSize":100},{"symbol":"ETHUSD_PERP","contractSize":10}]}"#,
        )
        .unwrap();
        let sizes = contract_sizes_from_info(info).unwrap();
        assert_eq!(
            trade_volumes(Market::Coinm, 50_000.0, 2.0, sizes.get(&Symbol::Btc).copied()).unwrap(),
            (0.004, 200.0)
        );
        assert_eq!(
            trade_volumes(Market::Coinm, 2_000.0, 3.0, sizes.get(&Symbol::Eth).copied()).unwrap(),
            (0.015, 30.0)
        );
    }

    #[test]
    fn spot_and_linear_quantities_remain_base_units() {
        for market in [Market::Spot, Market::Usdm] {
            assert_eq!(trade_volumes(market, 50_000.0, 2.0, None).unwrap(), (2.0, 100_000.0));
        }
    }

    #[test]
    fn invalid_or_missing_contract_metadata_is_rejected() {
        assert!(trade_volumes(Market::Coinm, 50_000.0, 2.0, None).is_err());
        assert!(trade_volumes(Market::Coinm, 0.0, 2.0, Some(100.0)).is_err());
        assert!(trade_volumes(Market::Spot, f64::NAN, 2.0, None).is_err());
        assert!(trade_volumes(Market::Spot, 50_000.0, -2.0, None).is_err());
        assert!(contract_sizes_from_info(ExchangeInfo { symbols: vec![] }).is_err());
    }
}

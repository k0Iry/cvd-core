//! Browser-host OP_BOOK negotiation and validation (native, no wasm needed).
use cvd_browser_host::contract::PluginSession;
use cvd_indicator_sdk::abi::{OP_BOOK, OP_HISTORY, STATUS_OK};
use cvd_indicator_sdk::wasm::WASM_ABI_VERSION;

fn desc(book: bool) -> serde_json::Value {
    serde_json::json!({"id": "t", "version": "1", "name": "t", "abi": 1, "context": {"bars": 0}, "params": [],
        "inputs": if book { serde_json::json!(["order_book"]) } else { serde_json::json!(["price"]) },
        "outputs": [{"key": "o", "pane": "separate", "type": if book { "price_levels" } else { "line" }}],
        "live": !book})
}
fn session(book: bool) -> PluginSession {
    let mut s = PluginSession::new();
    s.accept_abi(WASM_ABI_VERSION).unwrap();
    s.accept_descriptor(STATUS_OK, &serde_json::to_vec(&desc(book)).unwrap()).unwrap();
    s.accept_create(STATUS_OK, b"").unwrap();
    s
}
fn book(bid: f64, ask: f64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"source": {"exchange": "binance", "market": "usdm", "symbol": "BTC"}, "base": "BTC", "quote": "USDT",
        "generation": 1, "seq": 2, "exchange_time_ms": 3, "bids": [[bid, 1.0]], "asks": [[ask, 2.0]], "coverage": {"bid_floor": bid, "ask_ceiling": ask}})).unwrap()
}
fn levels(bb: f64, ba: f64, rows: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"unit": "BTC", "best_bid": bb, "best_ask": ba, "rows": rows})).unwrap()
}

#[test]
fn capability_negotiation_by_descriptor() {
    let mut bars = session(false);
    assert_eq!(
        bars.begin(OP_BOOK, &book(1.0, 2.0)).unwrap_err().code,
        "invalid_input",
        "bar plugin never gets OP_BOOK"
    );
    let mut b = session(true);
    assert_eq!(b.begin(OP_HISTORY, b"{}").unwrap_err().code, "invalid_input", "book plugin never gets bar ops");
    assert!(!b.is_poisoned() && !bars.is_poisoned(), "admission rejection is nonfatal");
}

#[test]
fn input_admitted_and_output_validated() {
    let mut s = session(true);
    assert_eq!(
        s.begin(OP_BOOK, &book(2.0, 1.0)).unwrap_err().code,
        "invalid_input",
        "crossed input refused before the plugin"
    );
    s.begin(OP_BOOK, &book(1.0, 2.0)).unwrap();
    let ok = s
        .accept_result(
            STATUS_OK,
            &levels(
                1.0,
                2.0,
                serde_json::json!([{"price": 2.0, "bid": 0.0, "ask": 2.0}, {"price": 1.0, "bid": 1.0, "ask": 0.0}]),
            ),
        )
        .unwrap();
    assert!(ok.contains("\"rows\""));
    // Lying best bid => invalid_output, instance poisoned (fatal).
    s.begin(OP_BOOK, &book(1.0, 2.0)).unwrap();
    let e = s
        .accept_result(STATUS_OK, &levels(1.5, 2.0, serde_json::json!([{"price": 1.0, "bid": 1.0, "ask": 0.0}])))
        .unwrap_err();
    assert_eq!(e.code, "invalid_output");
    assert!(s.is_poisoned());
}

#[test]
fn bar_descriptor_with_price_levels_rejected() {
    let mut s = PluginSession::new();
    s.accept_abi(WASM_ABI_VERSION).unwrap();
    let mut d = desc(false);
    d["outputs"][0]["type"] = "price_levels".into();
    assert_eq!(s.accept_descriptor(STATUS_OK, &serde_json::to_vec(&d).unwrap()).unwrap_err().code, "bad_descriptor");
}

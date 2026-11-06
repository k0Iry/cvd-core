// W: documented-optional `contracts` absent from real historical DRB rows
// (t_efb7830e). Witnesses are verbatim public rows committed with provenance in
// `tests/fixtures/drb-optional-contracts.json`; HTTP is a loopback replay and the
// workers/parsers/same-tape gates are the production code paths.
use super::*;

const FIXTURE: &str = include_str!("../tests/fixtures/drb-optional-contracts.json");

fn fx() -> Value {
    serde_json::from_str(FIXTURE).unwrap()
}
fn sym_key(s: Symbol) -> &'static str {
    if matches!(s, Symbol::Btc) { "BTC" } else { "ETH" }
}
fn rows(s: Symbol, side: &str) -> Vec<Value> {
    fx()[sym_key(s)][side].as_array().unwrap().clone()
}
fn size(s: Symbol) -> f64 {
    let base = sym_key(s);
    drb::validate_instrument(&fx()[base]["instrument"], drb::product(s), base).unwrap()
}
fn seq(v: &Value) -> u64 {
    v["trade_seq"].as_u64().unwrap()
}
const BOTH: [Symbol; 2] = [Symbol::Btc, Symbol::Eth];

#[test]
fn real_rows_without_optional_contracts_parse_like_archive_counterparts() {
    for s in BOTH {
        let (p, a, size) = (rows(s, "primary"), rows(s, "archive"), size(s));
        assert!(p[1].get("contracts").is_none(), "witness lacks contracts");
        assert!(a[1].get("contracts").is_some(), "control carries contracts");
        for (p, a) in p.iter().zip(&a) {
            let got = drb::parse_trade(p, drb::product(s), size).unwrap_or_else(|e| panic!("{s:?} {}: {e}", seq(p)));
            let want = drb::parse_trade(a, drb::product(s), size).unwrap();
            assert_eq!(
                (&got.id, got.timestamp_ms, got.price, got.base, got.quote, got.is_buy),
                (&want.id, want.timestamp_ms, want.price, want.base, want.quote, want.is_buy)
            );
        }
    }
}

#[test]
fn present_contracts_stay_strict_and_absent_path_keeps_unit_guards() {
    let s = Symbol::Btc;
    let (p, a, size) = (rows(s, "primary"), rows(s, "archive"), size(s));
    let name = drb::product(s);
    for bad in [json!(null), json!("1323"), json!(0.0), json!(-1323.0), json!(1324.0), json!(f64::MAX)] {
        let mut r = a[1].clone();
        r["contracts"] = bad.clone();
        let e = drb::parse_trade(&r, name, size).unwrap_err().to_string();
        assert!(e.contains("contracts") && e.contains("29230821"), "{bad}: {e}");
    }
    for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert!(drb::parse_trade(&p[1], name, bad).is_err(), "contract size {bad}");
    }
    let mut r = p[1].clone();
    r["amount"] = json!(-0.1323);
    assert!(drb::parse_trade(&r, name, size).is_err());
    // Required fields are not optional.
    for k in ["amount", "price", "direction", "trade_id", "timestamp", "trade_seq", "instrument_name"] {
        let mut r = p[1].clone();
        r.as_object_mut().unwrap().remove(k);
        assert!(drb::parse_trade(&r, name, size).is_err(), "{k}");
    }
}

/// LABELLED SYNTHETIC compatibility control (not a captured trade): a real
/// explicit archive row with positive fractional `contracts` and a consistent
/// `amount`. The baseline parser accepts it; removing only the optional
/// `contracts` property must parse identically — no lot/quantization rule.
#[test]
fn synthetic_fractional_contracts_parse_identically_with_or_without_optional_contracts() {
    for s in BOTH {
        let (name, size) = (drb::product(s), size(s));
        let mut explicit = rows(s, "archive")[1].clone();
        let contracts = explicit["contracts"].as_f64().unwrap() + 0.5;
        explicit["contracts"] = json!(contracts);
        explicit["amount"] = json!(contracts * size);
        let mut absent = explicit.clone();
        absent.as_object_mut().unwrap().remove("contracts");
        let want = drb::parse_trade(&explicit, name, size).unwrap_or_else(|e| panic!("{s:?} explicit: {e}"));
        let got = drb::parse_trade(&absent, name, size).unwrap_or_else(|e| panic!("{s:?} absent: {e}"));
        assert_eq!(
            (&got.id, got.timestamp_ms, got.price, got.base, got.quote, got.is_buy),
            (&want.id, want.timestamp_ms, want.price, want.base, want.quote, want.is_buy)
        );
    }
}

/// Real worker across before / contracts-absent / after; control replays the
/// explicit archive rows. Both must reach live with identical bars and CVD.
async fn primary_worker(s: Symbol, tape: Vec<Value>) -> (String, Option<String>, Vec<(i64, u64, i64, i64, i64)>, String, u64) {
    let size = size(s);
    let first = drb::parse_trade(&tape[0], drb::product(s), size).unwrap();
    let last = drb::parse_trade(&tape[2], drb::product(s), size).unwrap();
    let (base, _) = fixture(tape, 1).await;
    let state = Arc::new(crate::tests::state().await);
    let sh = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, s);
    {
        let mut e = sh.engine.lock().await;
        e.last_trade_id = Some((first.id.parse::<u64>().unwrap() - 1).to_string());
        e.last_trade_ms = Some(first.timestamp_ms - 1);
    }
    let (tx, rx) = mpsc::channel(8);
    let w = tokio::spawn(drb::worker(state.clone(), s, rx, base, size));
    let frontier = last.id.clone();
    tx.send(last).await.unwrap();
    // Progress witness: the committed cursor reaches the live frontier (or the
    // worker reports needs_backfill). Diagnostics start as `starting`, so state
    // alone is not used to infer progress.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if sh.engine.lock().await.last_trade_id.as_deref() == Some(frontier.as_str()) { break; }
            if sh.recovery.read().await.state == "needs_backfill" { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{s:?}: no cursor progress to {frontier} within 5s"));
    drop(tx);
    tokio::time::timeout(Duration::from_secs(5), w)
        .await
        .unwrap_or_else(|_| panic!("{s:?}: worker did not join within 5s"))
        .unwrap_or_else(|e| panic!("{s:?}: worker task failed: {e}"));
    let d = sh.recovery.read().await.clone();
    let e = sh.engine.lock().await;
    let bars = sh.for_tf(crate::types::Tf::M1).0.read().await.iter()
        .map(|b| (b.time, b.trades, (b.delta_usdt * 1e6).round() as i64, (b.buy_volume_base * 1e9).round() as i64, (b.cvd_usdt * 1e6).round() as i64))
        .collect();
    (format!("{} {:?}", d.state, d.detail), e.last_trade_id.clone(), bars, format!("{:?} {:?} cvd_closed={}", e.a1, e.sec, *sh.cvd_closed_usdt.read().await), e.a1.trades + e.sec.trades)
}

#[tokio::test]
async fn real_worker_crosses_absent_contracts_row_with_control_parity() {
    for s in BOTH {
        let (p, a) = (rows(s, "primary"), rows(s, "archive"));
        let got = primary_worker(s, p.clone()).await;
        let want = primary_worker(s, a).await;
        assert!(want.0.starts_with("live"), "control {s:?}: {}", want.0);
        assert!(got.0.starts_with("live"), "{s:?} stayed non-live: {}", got.0);
        assert_eq!(got.1.as_deref(), Some(seq(&p[2]).to_string().as_str()), "reached required frontier");
        assert_eq!(got.4, 3, "no trade dropped (flushed seconds + open second)");
        assert_eq!((got.1, got.2, got.3, got.4), (want.1, want.2, want.3, want.4));
    }
}

/// Loopback primary + `/archive` for the same-tape gate and archive chunks.
/// 50-row gate window: the three real rows plus labelled synthetic neighbours
/// derived from the explicit real row (sequence/timestamp/id shifted only).
async fn tape_server(s: Symbol, primary: Vec<Value>, archive: Vec<Value>) -> String {
    type Q = axum::extract::Query<std::collections::HashMap<String, String>>;
    let instrument = fx()[sym_key(s)]["instrument"].clone();
    let serve = |rows: Vec<Value>| {
        move |axum::extract::Query(q): Q| {
            let rows = rows.clone();
            async move {
                let (a, b) = (q["start_seq"].parse::<u64>().unwrap(), q["end_seq"].parse::<u64>().unwrap());
                let out: Vec<Value> = rows.into_iter().filter(|r| (a..=b).contains(&seq(r))).collect();
                axum::Json(json!({"jsonrpc":"2.0","result":{"trades":out,"has_more":false}}))
            }
        }
    };
    let app = axum::Router::new()
        .route("/get_last_trades_by_instrument", axum::routing::get(serve(primary)))
        .route("/archive/get_last_trades_by_instrument", axum::routing::get(serve(archive)))
        .route("/archive/get_instrument", axum::routing::get(move || {
            let i = instrument.clone();
            async move { axum::Json(json!({"jsonrpc":"2.0","result":i})) }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}
fn window(s: Symbol, side: &str) -> (Vec<Value>, u64) {
    let real = rows(s, side);
    let template = rows(s, "archive")[0].clone();
    let lo = seq(&real[0]) - 20;
    let mut out = Vec::new();
    for n in lo..lo + 50 {
        if let Some(r) = real.iter().find(|r| seq(r) == n) {
            out.push(r.clone());
            continue;
        }
        let mut r = template.clone();
        let d = n as i64 - seq(&template) as i64;
        r["trade_seq"] = json!(n);
        r["timestamp"] = json!(template["timestamp"].as_i64().unwrap() + d);
        r["trade_id"] = json!(format!("SYNTH-{n}"));
        out.push(r);
    }
    (out, lo + drb_gate_back())
}

#[tokio::test]
async fn same_tape_gate_accepts_presence_only_difference_and_rejects_conflicts() {
    let client = reqwest::Client::new();
    for s in BOTH {
        let size = size(s);
        let (p, target) = window(s, "primary");
        let (a, _) = window(s, "archive");
        let mid = seq(&rows(s, "primary")[1]);
        for (pp, aa) in [(p.clone(), a.clone()), (a.clone(), p.clone()), (p.clone(), p.clone())] {
            let base = tape_server(s, pp, aa).await;
            drb::archive_identity(&client, &base, &drb::archive_base(&base), s, size, target)
                .await
                .unwrap_or_else(|e| panic!("{s:?}: {e}"));
        }
        // Genuine identity conflicts remain fatal regardless of contracts presence.
        let i = p.iter().position(|r| seq(r) == mid).unwrap();
        for (field, val) in [("price", json!(1.0)), ("direction", json!("buy")), ("trade_id", json!("USDC-0")), ("timestamp", json!(1))] {
            let mut aa = a.clone();
            if field == "direction" && aa[i]["direction"] == "buy" { aa[i]["direction"] = json!("sell"); } else { aa[i][field] = val; }
            let base = tape_server(s, p.clone(), aa).await;
            let e = drb::archive_identity(&client, &base, &drb::archive_base(&base), s, size, target).await.unwrap_err();
            assert!(matches!(e, drb::HistoryError::Fatal(_)), "{field}: {e}");
        }
        // Both present and conflicting (self-consistent lots, different quantity).
        let mut aa = a.clone();
        aa[i]["contracts"] = json!(aa[i]["contracts"].as_f64().unwrap() + 1.0);
        aa[i]["amount"] = json!(aa[i]["contracts"].as_f64().unwrap() * size);
        let mut pp = p.clone();
        pp[i]["contracts"] = a[i]["contracts"].clone();
        let base = tape_server(s, pp, aa).await;
        assert!(matches!(
            drb::archive_identity(&client, &base, &drb::archive_base(&base), s, size, target).await,
            Err(drb::HistoryError::Fatal(_))
        ));
        // Malformed present contracts on the archive side.
        let mut aa = a.clone();
        aa[i]["contracts"] = json!(null);
        let base = tape_server(s, p.clone(), aa).await;
        assert!(matches!(
            drb::archive_identity(&client, &base, &drb::archive_base(&base), s, size, target).await,
            Err(drb::HistoryError::Fatal(_))
        ));
    }
}

#[tokio::test]
async fn archive_duplicates_validate_each_row_and_allow_presence_only_difference() {
    let client = reqwest::Client::new();
    for s in BOTH {
        let size = size(s);
        let (p, a) = (rows(s, "primary"), rows(s, "archive"));
        let (first, last) = (drb::parse_trade(&a[0], drb::product(s), size).unwrap(), seq(&a[2]));
        let chunk = |tape: Vec<Value>| async {
            let base = tape_server(s, vec![], tape).await;
            drb::archive_chunk(&client, &drb::archive_base(&base), s, size, Some(seq(&a[0]) - 1), Some(first.timestamp_ms),
                last + 1, first.timestamp_ms + 30 * 3_600_000).await
        };
        let want = chunk(a.clone()).await.unwrap();
        assert_eq!(want.len(), 3);
        for dup in [p[1].clone(), a[1].clone()] {
            for tape in [vec![a[0].clone(), a[1].clone(), dup.clone(), a[2].clone()], vec![a[0].clone(), p[1].clone(), dup, a[2].clone()]] {
                let got = chunk(tape).await.unwrap_or_else(|e| panic!("{s:?}: {e}"));
                assert_eq!(got.iter().map(|t| (&t.id, t.base, t.quote, t.is_buy)).collect::<Vec<_>>(),
                    want.iter().map(|t| (&t.id, t.base, t.quote, t.is_buy)).collect::<Vec<_>>());
            }
        }
        // An invalid duplicate cannot hide behind a good earlier row.
        for (k, v) in [("contracts", json!(null)), ("contracts", json!("x")), ("contracts", json!(1.0)), ("price", json!(1.0))] {
            let mut bad = a[1].clone();
            bad[k] = v.clone();
            let e = chunk(vec![a[0].clone(), a[1].clone(), bad, a[2].clone()]).await.unwrap_err();
            assert!(matches!(e, drb::HistoryError::Fatal(_)), "{k}={v}: {e}");
        }
        // Both present and conflicting (self-consistent) is still a conflicting duplicate.
        let mut bad = a[1].clone();
        bad["contracts"] = json!(a[1]["contracts"].as_f64().unwrap() + 1.0);
        bad["amount"] = json!(bad["contracts"].as_f64().unwrap() * size);
        let e = chunk(vec![a[0].clone(), a[1].clone(), bad, a[2].clone()]).await.unwrap_err();
        assert!(e.to_string().contains("conflicting duplicate"), "{e}");
    }
}

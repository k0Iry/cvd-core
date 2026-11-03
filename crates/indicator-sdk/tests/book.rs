//! Order-book (`OP_BOOK`) SDK contract: descriptor capability rules, input
//! admission, output validation and the target-independent wasm glue.
use cvd_indicator_sdk::abi::{OP_BOOK, OP_HISTORY, STATUS_ERROR, STATUS_INVALID_ARGUMENT, STATUS_OK};
use cvd_indicator_sdk::book::*;
use cvd_indicator_sdk::*;

pub fn book_desc() -> Descriptor {
    Descriptor {
        id: "test.book".into(),
        version: "0.0.1".into(),
        name: "Book".into(),
        description: String::new(),
        abi: 1,
        context: ContextSpec::default(),
        params: vec![],
        inputs: vec![InputField::OrderBook],
        outputs: vec![OutputSpec {
            key: "ladder".into(),
            label: String::new(),
            pane: Pane::Separate,
            pane_group: None,
            colors: vec![],
            widths: vec![],
            readout_only: false,
            kind: OutputKind::PriceLevels,
        }],
        live: false,
    }
}

pub fn snapshot() -> BookSnapshot {
    BookSnapshot {
        source: BookSource { exchange: "binance".into(), market: "usdm".into(), symbol: "BTC".into() },
        base: "BTC".into(),
        quote: "USDT".into(),
        generation: 1,
        seq: 5,
        exchange_time_ms: 1,
        bids: vec![[100.0, 1.0], [99.0, 2.0]],
        asks: vec![[101.0, 3.0], [102.0, 4.0]],
        coverage: BookCoverage { bid_floor: 99.0, ask_ceiling: 102.0 },
    }
}

#[test]
fn book_descriptor_rules() {
    book_desc().validate().unwrap();
    assert!(book_desc().is_book());
    let mut d = book_desc();
    d.live = true;
    assert!(d.validate().is_err(), "book plugins are not bar-live");
    let mut d = book_desc();
    d.inputs.push(InputField::Price);
    assert!(d.validate().is_err(), "order_book is exclusive");
    let mut d = book_desc();
    d.outputs[0].kind = OutputKind::Line;
    assert!(d.validate().is_err(), "needs price_levels output");
    let mut d = book_desc();
    d.inputs = vec![InputField::Price];
    d.live = true;
    assert!(d.validate().is_err(), "price_levels without order_book rejected");
    // JSON wire names
    let j = serde_json::to_value(book_desc()).unwrap();
    assert_eq!(j["inputs"][0], "order_book");
    assert_eq!(j["outputs"][0]["type"], "price_levels");
}

#[test]
fn snapshot_admission() {
    snapshot().check().unwrap();
    let mut s = snapshot();
    s.bids[0][0] = 101.0;
    assert!(s.check().is_err(), "crossed");
    let mut s = snapshot();
    s.bids.swap(0, 1);
    assert!(s.check().is_err(), "unordered");
    let mut s = snapshot();
    s.asks[1][1] = 0.0;
    assert!(s.check().is_err(), "zero qty is not a level");
    let mut s = snapshot();
    s.bids.clear();
    assert!(s.check().is_err(), "empty side");
    let mut s = snapshot();
    s.coverage.bid_floor = 99.5;
    assert!(s.check().is_err(), "level outside coverage");
    // Noncanonical-input hardening (C-04): bounds must be positive and finite.
    for (lo, hi) in [(0.0, 102.0), (-1.0, 102.0), (99.0, f64::INFINITY), (f64::NAN, 102.0)] {
        let mut s = snapshot();
        s.coverage = BookCoverage { bid_floor: lo, ask_ceiling: hi };
        assert!(s.check().is_err(), "coverage {lo}..{hi}");
    }
    let mut s = snapshot();
    s.source.market = "us dm".into();
    assert!(s.check().is_err(), "identity");
}

#[test]
fn output_validation() {
    let b = snapshot();
    let ok = PriceLevels {
        unit: "BTC".into(),
        best_bid: 100.0,
        best_ask: 101.0,
        rows: vec![
            PriceLevelRow { price: 101.0, bid: 0.0, ask: 3.0 },
            PriceLevelRow { price: 100.0, bid: 1.0, ask: 0.0 },
        ],
    };
    ok.validate(&b).unwrap();
    let mut o = ok.clone();
    o.best_bid = 99.0;
    assert!(o.validate(&b).is_err(), "best must echo input");
    let mut o = ok.clone();
    o.rows.reverse();
    assert!(o.validate(&b).is_err(), "descending");
    let mut o = ok.clone();
    o.rows[0].ask = f64::NAN;
    assert!(o.validate(&b).is_err(), "finite");
    let mut o = ok.clone();
    o.rows = vec![o.rows[0]; MAX_PRICE_LEVEL_ROWS + 1];
    assert!(o.validate(&b).is_err(), "row bound");
    let mut o = ok;
    o.unit = "B T C".into();
    assert!(o.validate(&b).is_err(), "unit label");
}

struct Echo;
impl BookIndicator for Echo {
    fn descriptor() -> Descriptor {
        book_desc()
    }
    fn new(_: &ParamValues) -> Result<Self, PluginError> {
        Ok(Echo)
    }
    fn compute_book(&mut self, b: &BookSnapshot) -> Result<PriceLevels, PluginError> {
        let rows = b
            .asks
            .iter()
            .rev()
            .map(|l| PriceLevelRow { price: l[0], bid: 0.0, ask: l[1] })
            .chain(b.bids.iter().map(|l| PriceLevelRow { price: l[0], bid: l[1], ask: 0.0 }))
            .collect();
        Ok(PriceLevels { unit: b.base.clone(), best_bid: b.bids[0][0], best_ask: b.asks[0][0], rows })
    }
}

#[test]
fn glue_lifecycle_and_op_gating() {
    let mut g = BookGlue::<Echo>::new();
    assert_eq!(g.invoke(OP_BOOK, b"{}"), STATUS_INVALID_ARGUMENT, "before create");
    assert_eq!(g.describe(), STATUS_OK);
    assert_eq!(g.create(b"{}"), STATUS_OK);
    assert_eq!(g.create(b"{}"), STATUS_INVALID_ARGUMENT);
    assert_eq!(g.invoke(OP_HISTORY, b"{}"), STATUS_INVALID_ARGUMENT, "bar op refused");
    let body = serde_json::to_vec(&snapshot()).unwrap();
    assert_eq!(g.invoke(OP_BOOK, &body), STATUS_OK);
    let out: PriceLevels = serde_json::from_slice(g.result()).unwrap();
    assert_eq!(out.rows.len(), 4);
    let mut bad = snapshot();
    bad.asks[0][0] = 99.5;
    assert_eq!(g.invoke(OP_BOOK, &serde_json::to_vec(&bad).unwrap()), STATUS_ERROR);
    let e: PluginError = serde_json::from_slice(g.result()).unwrap();
    assert_eq!(e.code, codes::INVALID_INPUT);
    g.destroy();
    assert_eq!(g.invoke(OP_BOOK, &body), STATUS_INVALID_ARGUMENT);
}

#[test]
fn bar_glue_refuses_op_book() {
    // Existing bar plugins never accept OP_BOOK (old modules keep their contract).
    struct Bars;
    impl Indicator for Bars {
        fn descriptor() -> Descriptor {
            let mut d = book_desc();
            d.inputs = vec![InputField::Price];
            d.outputs[0].kind = OutputKind::Line;
            d.live = true;
            d
        }
        fn new(_: &ParamValues) -> Result<Self, PluginError> {
            Ok(Bars)
        }
        fn compute(&mut self, _: &ComputeContext<'_>, b: &[InputBar], f: usize) -> Result<SeriesSet, PluginError> {
            Ok([("ladder".to_string(), SeriesValues::Line { values: vec![None; b.len() - f] })].into())
        }
    }
    let mut g = cvd_indicator_sdk::wasm::WasmGlue::<Bars>::new();
    assert_eq!(g.create(b"{}"), STATUS_OK);
    assert_eq!(g.invoke(OP_BOOK, &serde_json::to_vec(&snapshot()).unwrap()), STATUS_INVALID_ARGUMENT);
}

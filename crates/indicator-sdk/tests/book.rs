//! Order-book (`OP_BOOK`) SDK contract: descriptor capability rules, input
//! admission, output validation and the target-independent wasm glue.
use cvd_indicator_sdk::abi::{OP_BOOK, OP_HISTORY, OP_LIVE_INIT, STATUS_ERROR, STATUS_INVALID_ARGUMENT, STATUS_OK};
use cvd_indicator_sdk::book::*;
use cvd_indicator_sdk::wasm::WasmGlue;
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
impl Indicator for Echo {
    type Domain = Book;
    fn descriptor() -> Descriptor {
        book_desc()
    }
    fn new(_: &ParamValues) -> Result<Self, PluginError> {
        Ok(Echo)
    }
    fn compute(&mut self, b: &BookSnapshot) -> Result<PriceLevels, PluginError> {
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
    let mut g = WasmGlue::<Echo>::new();
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

/// Book indicator with mutable state: the unit label encodes the
/// per-instance call count, so instance reuse/reset is directly observable.
struct Tally(u32);
impl Indicator for Tally {
    type Domain = Book;
    fn descriptor() -> Descriptor {
        book_desc()
    }
    fn new(_: &ParamValues) -> Result<Self, PluginError> {
        Ok(Tally(0))
    }
    fn compute(&mut self, b: &BookSnapshot) -> Result<PriceLevels, PluginError> {
        self.0 += 1;
        let mut out = Echo.compute(b)?;
        out.unit = format!("N{}", self.0);
        Ok(out)
    }
}

#[test]
fn book_instance_persists_across_snapshots_until_destroy() {
    let body = serde_json::to_vec(&snapshot()).unwrap();
    let unit = |g: &WasmGlue<Tally>| serde_json::from_slice::<PriceLevels>(g.result()).unwrap().unit;
    let mut g = WasmGlue::<Tally>::new();
    assert_eq!(g.create(b"{}"), STATUS_OK);
    for n in 1..=3 {
        assert_eq!(g.invoke(OP_BOOK, &body), STATUS_OK);
        assert_eq!(unit(&g), format!("N{n}"));
    }
    // A rejected snapshot never reaches compute.
    let mut bad = snapshot();
    bad.asks[0][0] = 99.5;
    assert_eq!(g.invoke(OP_BOOK, &serde_json::to_vec(&bad).unwrap()), STATUS_ERROR);
    assert_eq!(g.invoke(OP_BOOK, &body), STATUS_OK);
    assert_eq!(unit(&g), "N4");
    assert_eq!(g.invoke(OP_LIVE_INIT, &body), STATUS_INVALID_ARGUMENT, "wrong domain op");
    g.destroy();
    assert_eq!(g.create(b"{}"), STATUS_OK);
    assert_eq!(g.invoke(OP_BOOK, &body), STATUS_OK);
    assert_eq!(unit(&g), "N1", "recreate starts a fresh instance");
}

#[test]
fn book_describe_requires_book_descriptor() {
    struct NotBook;
    impl Indicator for NotBook {
        type Domain = Book;
        fn descriptor() -> Descriptor {
            let mut d = book_desc();
            d.inputs = vec![InputField::Price];
            d
        }
        fn new(_: &ParamValues) -> Result<Self, PluginError> {
            Ok(NotBook)
        }
        fn compute(&mut self, b: &BookSnapshot) -> Result<PriceLevels, PluginError> {
            Echo.compute(b)
        }
    }
    let mut g = WasmGlue::<NotBook>::new();
    assert_eq!(g.describe(), STATUS_ERROR);
    let e: PluginError = serde_json::from_slice(g.result()).unwrap();
    assert_eq!(e.code, codes::INCOMPATIBLE);
}

/// API-01: descriptor/domain agreement at BOTH describe and create (incl.
/// create without describe), for both domains, before any constructor runs.
mod domain_agreement {
    use super::*;
    use cvd_indicator_sdk::abi::OP_LIVE_UPDATE;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn bar_desc() -> Descriptor {
        let mut d = book_desc();
        d.id = "test.bar".into();
        d.inputs = vec![InputField::Price];
        d.outputs[0].kind = OutputKind::Line;
        d.live = true;
        d
    }

    static BARS_NEW: AtomicUsize = AtomicUsize::new(0);
    struct BarsWithBookDesc;
    impl Indicator for BarsWithBookDesc {
        type Domain = Bars;
        fn descriptor() -> Descriptor {
            book_desc()
        }
        fn new(_: &ParamValues) -> Result<Self, PluginError> {
            BARS_NEW.fetch_add(1, Ordering::SeqCst);
            Ok(Self)
        }
        fn compute(&mut self, _: BarsInput<'_>) -> Result<SeriesSet, PluginError> {
            unreachable!("compute on domain-mismatched instance")
        }
    }

    static BOOK_NEW: AtomicUsize = AtomicUsize::new(0);
    struct BookWithBarDesc;
    impl Indicator for BookWithBarDesc {
        type Domain = Book;
        fn descriptor() -> Descriptor {
            bar_desc()
        }
        fn new(_: &ParamValues) -> Result<Self, PluginError> {
            BOOK_NEW.fetch_add(1, Ordering::SeqCst);
            Ok(Self)
        }
        fn compute(&mut self, _: &BookSnapshot) -> Result<PriceLevels, PluginError> {
            unreachable!("compute on domain-mismatched instance")
        }
    }

    fn incompatible<P: Indicator>(g: &WasmGlue<P>) {
        let e: PluginError = serde_json::from_slice(g.result()).unwrap();
        assert_eq!(e.code, codes::INCOMPATIBLE);
    }

    #[test]
    fn fixtures_are_individually_valid() {
        bar_desc().validate().unwrap();
        book_desc().validate().unwrap();
    }

    #[test]
    fn bars_domain_rejects_book_descriptor() {
        let mut g = WasmGlue::<BarsWithBookDesc>::new();
        assert_eq!(g.describe(), STATUS_ERROR, "describe");
        incompatible(&g);
        let mut g = WasmGlue::<BarsWithBookDesc>::new();
        assert_eq!(g.create(b"{}"), STATUS_ERROR, "create without describe");
        incompatible(&g);
        assert_eq!(BARS_NEW.load(Ordering::SeqCst), 0, "constructor must not run");
        for op in [OP_HISTORY, OP_LIVE_INIT, OP_LIVE_UPDATE, OP_BOOK] {
            assert_eq!(g.invoke(op, b"{}"), STATUS_INVALID_ARGUMENT, "no instance, op {op}");
        }
    }

    #[test]
    fn book_domain_rejects_bar_descriptor() {
        let mut g = WasmGlue::<BookWithBarDesc>::new();
        assert_eq!(g.describe(), STATUS_ERROR, "describe");
        incompatible(&g);
        let mut g = WasmGlue::<BookWithBarDesc>::new();
        assert_eq!(g.create(b"{}"), STATUS_ERROR, "create without describe");
        incompatible(&g);
        assert_eq!(BOOK_NEW.load(Ordering::SeqCst), 0, "constructor must not run");
        let body = serde_json::to_vec(&snapshot()).unwrap();
        assert_eq!(g.invoke(OP_BOOK, &body), STATUS_INVALID_ARGUMENT, "no instance");
    }

    #[test]
    fn matching_domain_create_destroy_recreate() {
        let mut g = WasmGlue::<Echo>::new();
        assert_eq!(g.create(b"{}"), STATUS_OK, "create without describe ok");
        g.destroy();
        assert_eq!(g.create(b"{}"), STATUS_OK, "recreate");
        let body = serde_json::to_vec(&snapshot()).unwrap();
        assert_eq!(g.invoke(OP_BOOK, &body), STATUS_OK);
        assert_eq!(g.invoke(OP_HISTORY, b"{}"), STATUS_INVALID_ARGUMENT, "wrong-op guard");
    }
}

/// M1: a genuine 5000-level-per-side spot snapshot is admitted; 5001 is not.
#[test]
fn snapshot_admits_5000_levels_rejects_5001() {
    let deep = |n: usize| {
        let mut s = snapshot();
        s.bids = (0..n).map(|i| [50_000.0 - 0.01 * (i as f64 + 1.0), 1.0]).collect();
        s.asks = (0..n).map(|i| [50_000.0 + 0.01 * (i as f64 + 1.0), 1.0]).collect();
        s.coverage = BookCoverage { bid_floor: s.bids[n - 1][0], ask_ceiling: s.asks[n - 1][0] };
        s
    };
    assert_eq!(MAX_BOOK_LEVELS, 5000);
    assert!(deep(5000).check().is_ok());
    assert!(deep(5001).check().is_err());
}

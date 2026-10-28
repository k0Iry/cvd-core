use cvd_indicator_sdk::*;
use std::collections::BTreeMap;

fn desc() -> Descriptor {
    Descriptor {
        id: "t.x".into(),
        version: "1.0.0".into(),
        name: "x".into(),
        description: String::new(),
        abi: abi::ABI_VERSION,
        context: ContextSpec { bars: 10, per_tf: [("1h".to_string(), 3)].into() },
        params: vec![
            ParamSpec { key: "n".into(), label: String::new(), kind: ParamKind::Int { default: 5, min: 1, max: 9 } },
            ParamSpec {
                key: "m".into(),
                label: String::new(),
                kind: ParamKind::Choice { default: "a".into(), options: vec!["a".into(), "b".into()] },
            },
        ],
        inputs: vec![InputField::Price],
        outputs: vec![
            OutputSpec {
                key: "l".into(),
                label: String::new(),
                pane: Pane::Price,
                pane_group: None,
                colors: vec![],
                kind: OutputKind::Line,
            },
            OutputSpec {
                key: "s".into(),
                label: String::new(),
                pane: Pane::Price,
                pane_group: None,
                colors: vec![],
                kind: OutputKind::SegmentedLines { layers: 2, tail_marker: true },
            },
        ],
        live: true,
    }
}

#[test]
fn descriptor_json_shape_is_stable() {
    let d = desc();
    d.validate().unwrap();
    let v = serde_json::to_value(&d).unwrap();
    assert_eq!(v["params"][0]["type"], "int");
    assert_eq!(v["outputs"][1]["type"], "segmented_lines");
    assert_eq!(v["outputs"][1]["layers"], 2);
    assert_eq!(serde_json::from_value::<Descriptor>(v).unwrap(), d);
    assert_eq!(d.context.bars_for("1h"), 3);
    assert_eq!(d.context.bars_for("1m"), 10);
}

#[test]
fn descriptor_rejections() {
    let mut d = desc();
    d.id = "Bad Id".into();
    assert!(d.validate().is_err());
    let mut d = desc();
    d.abi = 2;
    assert!(d.validate().is_err());
    let mut d = desc();
    d.outputs[1].key = "l".into();
    assert!(d.validate().is_err());
    let mut d = desc();
    d.context.bars = 1_000_000;
    assert!(d.validate().is_err());
}

#[test]
fn params_defaults_and_validation() {
    let d = desc();
    let p = resolve_params(&d, b"").unwrap();
    assert_eq!(p.int("n"), Some(5));
    assert_eq!(p.choice("m"), Some("a"));
    assert_eq!(resolve_params(&d, br#"{"n":9,"m":"b"}"#).unwrap().int("n"), Some(9));
    for bad in [&br#"{"n":10}"#[..], br#"{"m":"c"}"#, br#"{"z":1}"#, br#"{"n":1.5}"#, b"nope"] {
        assert_eq!(resolve_params(&d, bad).unwrap_err().code, codes::INVALID_PARAMS);
    }
}

#[test]
fn output_validation_is_strict() {
    let d = desc();
    let mut s = BTreeMap::new();
    s.insert("l".to_string(), SeriesValues::Line { values: vec![Some(1.0), None] });
    s.insert("s".to_string(), SeriesValues::SegmentedLines { layers: vec![vec![None, Some(2.0)], vec![None, None]] });
    let o = Output { times: vec![1, 2], series: s.clone() };
    o.validate(&d, Some(&[1, 2])).unwrap();
    assert!(o.validate(&d, Some(&[1, 3])).is_err());
    let mut bad = o.clone();
    bad.times = vec![2, 1];
    assert!(bad.validate(&d, None).is_err());
    let mut bad = o.clone();
    bad.series.insert("s".into(), SeriesValues::SegmentedLines { layers: vec![vec![None, None]] });
    assert!(bad.validate(&d, None).is_err());
    let mut bad = o.clone();
    bad.series.insert("l".into(), SeriesValues::Histogram { values: vec![None, None] });
    assert!(bad.validate(&d, None).is_err());
    let mut bad = o.clone();
    bad.series.insert("extra".into(), SeriesValues::Line { values: vec![None, None] });
    assert!(bad.validate(&d, None).is_err());
    let mut bad = o;
    bad.series.remove("s");
    assert!(bad.validate(&d, None).is_err());
}

#[test]
fn error_messages_are_bounded() {
    let e = PluginError::compute("é".repeat(1000));
    assert!(e.message.len() <= MAX_ERROR_MESSAGE);
}

#[test]
fn live_op_wire_shape() {
    let op = LiveOp::Replace { bar: InputBar { time: 5, ..Default::default() } };
    let v = serde_json::to_value(&op).unwrap();
    assert_eq!(v["op"], "replace");
    assert_eq!(v["bar"]["time"], 5);
    assert!(v["bar"].get("lt").is_none());
}

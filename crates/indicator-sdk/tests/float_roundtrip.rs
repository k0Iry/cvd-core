//! Regression: f64 values crossing the JSON wire must round-trip bit-exactly.
//!
//! serde_json's default float parser is not correctly rounded; the SDK (and
//! host/runner) enable `serde_json/float_roundtrip` themselves so exactness
//! never depends on a downstream crate's feature unification. This test is
//! built with `-p cvd-indicator-sdk` alone to prove that.

use cvd_indicator_sdk::model::SeriesValues;

/// Deterministic xorshift generator of finite f64 bit patterns.
fn values(n: usize) -> Vec<f64> {
    let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let v = f64::from_bits(s);
        if v.is_finite() {
            out.push(v);
        }
        // Also exercise price-like magnitudes.
        let p = (s >> 11) as f64 / (1u64 << 53) as f64 * 100_000.0;
        out.push(p);
    }
    out.truncate(n);
    out
}

#[test]
fn f64_json_bit_roundtrip() {
    let vs = values(200_000);
    let text = serde_json::to_string(&vs).unwrap();
    let back: Vec<f64> = serde_json::from_str(&text).unwrap();
    let bad = vs.iter().zip(&back).filter(|(a, b)| a.to_bits() != b.to_bits()).count();
    assert_eq!(bad, 0, "{bad} of {} f64 values drifted through JSON", vs.len());
}

#[test]
fn series_values_bit_roundtrip() {
    let vs: Vec<Option<f64>> = values(20_000).into_iter().map(Some).collect();
    let col = SeriesValues::Line { values: vs.clone() };
    let back: SeriesValues = serde_json::from_slice(&serde_json::to_vec(&col).unwrap()).unwrap();
    let SeriesValues::Line { values: got } = back else { panic!("variant changed") };
    let bad = vs.iter().zip(&got).filter(|(a, b)| a.map(f64::to_bits) != b.map(f64::to_bits)).count();
    assert_eq!(bad, 0, "{bad} series values drifted through JSON");
}

# CVD plugin JSON reference (v1)

Shared data types for SDK and browser-WASM indicators. For build/import steps,
use the [authoring guide](https://github.com/k0Iry/cvd-indicators/blob/master/docs/authoring.md);
for module exports, limits and isolation, use the [WASM contract](plugin-wasm-v1.md).
The server does not load plugins or ship a native runner.

Definitions: [`model.rs`](../crates/indicator-sdk/src/model.rs) and
[`author.rs`](../crates/indicator-sdk/src/author.rs). `ComputeContext.phase`
explicitly distinguishes `History`, `LiveInit` and `LiveUpdate`; do not infer
phase from call counts. It is an in-process SDK field, not a JSON field.
The SDK enables `serde_json/float_roundtrip` for bit-exact finite `f64` values.

## 4. Descriptor

```json
{ "id": "sample.channel", "version": "1.0.0", "name": "…", "description": "…", "abi": 1,
  "context": { "bars": 500, "per_tf": { "1h": 300 } },
  "params":  [ { "key": "length", "label": "Length", "type": "int", "default": 20, "min": 1, "max": 500 } ],
  "inputs":  [ "price", "aggregate_price", "delta", "aggregate_delta" ],
  "outputs": [ { "key": "sma", "label": "SMA", "pane": "price", "pane_group": null, "colors": ["#f5a623"], "type": "line" },
               { "key": "channel", "pane": "price", "type": "dual_range" },
               { "key": "delta", "pane": "separate", "pane_group": "flow", "type": "histogram" },
               { "key": "extremes", "pane": "price", "widths": [2, 1], "type": "segmented_lines", "layers": 2, "tail_marker": true },
               { "key": "total", "label": "Total (quote)", "pane": "separate", "pane_group": "flow", "readout_only": true, "type": "line" } ],
  "live": true }
```

* `id`: `[a-z0-9_.-]{1,64}`. Param types: `int | float | bool | choice`
  (`options`). Unknown parameter keys are rejected.
* `context.bars` (optionally per timeframe `1m 5m 15m 30m 1h`) is the bounded
  warm-up the host supplies **before** the first requested output bar. Max 8192.
  Required context comes from this declaration, never from host constants.
* Output kinds: `line`, `histogram`, `dual_range` (upper/lower band),
  `segmented_lines` (`layers` 1..=32 independent horizontal step lines; `null`
  breaks a segment; `tail_marker` asks the renderer to label the last value).
  Renderers dispatch on kind, never on indicator id.
* Optional presentation metadata (v1, backward compatible; omitted = default):
  * `widths: [u8]` — line widths in px, each 1..=16. `[]` = renderer default,
    one value = whole series, for `segmented_lines` exactly `layers` values =
    per-layer widths.
  * `readout_only: bool` (default `false`) — the output is an ordinary `line`
    column that is **not drawn**; it is shown only in readouts/legends with its
    label and pane (e.g. a quote-volume Total). Only valid with `type: line`.
  Invalid descriptors are rejected; browser-host errors are defined in the
  [WASM contract](plugin-wasm-v1.md#6-runtime-failures-and-isolation).

## 5. Canonical input (`InputBar`)

`time` is the canonical CVD bar time exactly as stored and charted: unix
**seconds of the last second of the bucket** (1m bucket starting 00:00 has
`time` = 00:00:59). Treat it as an opaque strictly increasing key. Names reuse
the established CVD bar wire names:

| field | meaning |
|---|---|
| `time` | canonical bar time (bucket-end second) |
| `po ph pl price_close` | source OHLC |
| `apo aph apl apc` | aggregate OHLC |
| `cvd_usdt` / `ac` | established cumulative CVD (source / aggregate) from persistence — plugins project it, never recompute it |
| `delta_usdt` / `ad` | per-bar delta (source / aggregate) |
| `bvb svb` / `abvb asvb` | base buy/sell volume (source / aggregate) |
| `buy_volume_usdt sell_volume_usdt` / `agb ags` | quote buy/sell volume |
| `trades` | trade count |

Indicator outputs (e.g. historical `lt/lb/alt/alb`) are **not** input. Source
quality observation stays in core and is not part of plugin input. Plugins
receive no credentials, SQL, browser or network authority.

`Scope = { exchange, market, symbol, tf, mode: "source" | "aggregate" }`.

## 6. Operations (bodies are JSON)

* `HISTORY` — `{scope, revision, output_from, bars}`: stateless. A fresh
  instance is created, invoked once and destroyed. `bars[..output_from]` are
  warm-up; output `times` must equal `bars[output_from..].time`.
* `LIVE_INIT` — `{scope, revision, bars}`: creates persistent live state with a
  bounded window (`context + 1 + 64` bars kept). Output: the last bar.
* `LIVE_UPDATE` — `{revision, seq, ops:[{op:"append"|"replace", bar}]}`.
  `seq` must be exactly previous + 1 (init = 0) else `sequence_gap`; revision
  mismatch → `stale_generation`. `append` must be strictly after the last bar;
  `replace` overwrites a bar in the window (never accumulates). A rejected
  delta leaves state unchanged. Output covers first touched bar → window end.

Output is `{times, series}`. `series` is keyed by descriptor output key; its
set of keys must match the descriptor exactly. Each entry has a `type` and:

| Type | Value fields |
|---|---|
| `line`, `histogram` | `values` |
| `dual_range` | `upper`, `lower` |
| `segmented_lines` | `layers` (one array per declared layer) |

Every value array aligns one-for-one with `times`; values are finite numbers
or `null`, never NaN/Infinity. Errors use `PluginError {code, message}`; messages
are limited to 512 bytes and must not expose host diagnostics. Browser-host
revision guards and failures are specified in the
[WASM contract](plugin-wasm-v1.md#5-history-and-live-semantics).

## Authoring

Implement `cvd_indicator_sdk::Indicator` once, choosing the data domain, and
export it with `export_indicator!`:

```rust
impl Indicator for MyType {
    type Domain = Bars; // or Book
    fn descriptor() -> Descriptor { /* ... */ }
    fn new(params: &ParamValues) -> Result<Self, PluginError> { /* ... */ }
    fn compute(&mut self, i: BarsInput<'_>) -> Result<SeriesSet, PluginError> { /* ... */ }
}
export_indicator!(Bars: MyType);
```

A `Book` indicator's `compute` takes `&BookSnapshot` and returns `PriceLevels`.
The macro emits the [WASM ABI](plugin-wasm-v1.md) exports on `wasm32` only;
there is no native C ABI. Native tests drive `author::Instance` or
`wasm::WasmGlue` directly. A module's descriptor must match its `Domain`
(a `Book` descriptor declares `order_book`; a `Bars` one must not): both
`cvd_describe` and `cvd_create` reject a mismatch as `incompatible`.

Source written against an earlier SDK (the former `BookIndicator` trait and
legacy macros) must migrate to this trait and rebuild to use the current SDK.
The WASM v1 binary ABI is unchanged, so the host does not deliberately reject
previously built modules; they are simply not a supported or tested target.

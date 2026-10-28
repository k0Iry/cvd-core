# CVD indicator plugin API v1

Status: **frozen for v1** (ABI_VERSION = 1, PROTOCOL_VERSION = 1). Changes go
through the parent maintainer; additive changes must keep v1 libraries loading.

Authoritative source: `crates/indicator-sdk` (`abi.rs`, `model.rs`,
`protocol.rs`, `author.rs`). This document explains it; if the two disagree,
the code is a bug to report, not a fork to follow.

## 1. Architecture

```
cvd-core host  ──frames(stdin/stdout)──►  cvd-indicator-runner  ──C ABI──►  lib<indicator>.so
 (ingestion,                               (one process per                (Rust cdylib built
  persistence,                              configured library;             with the SDK)
  HTTP/WS)                                  empty env, cwd=/)
```

* The host **never** `dlopen`s an indicator. Libraries are loaded only by the
  runner, a separate process. Crash / abort / segfault / hang of library code
  kills at most that runner. The host kills runners that exceed a deadline.
* A subprocess is **not** a security sandbox: the library runs with the
  runner's OS privileges. Only operator-owned paths from host configuration
  are ever loaded; no path or URL is accepted from users/clients.
* No hot unloading: the library stays mapped until the runner exits.

## 2. C ABI (library side)

Export (the SDK macro does this):

```c
const CvdPluginApiV1* cvd_plugin_entry_v1(uint32_t host_abi_version);
```

Return NULL if `host_abi_version` is unsupported. The table:

```c
typedef struct { uint8_t* ptr; size_t len; size_t cap; } CvdBuffer;   // library-owned
typedef struct {
  uint32_t abi_version;   // must be 1
  uint32_t struct_size;   // >= sizeof(v1 table); runner rejects smaller
  int32_t (*describe)(CvdBuffer* out);                                   // Descriptor JSON
  int32_t (*create)(const uint8_t* params, size_t len, void** out_handle, CvdBuffer* out_err);
  int32_t (*invoke)(void* handle, uint32_t op, const uint8_t* in, size_t len, CvdBuffer* out);
  void    (*destroy)(void* handle);
  void    (*free_buffer)(CvdBuffer buf);                                 // frees any buffer above
} CvdPluginApiV1;
```

Status codes: `0 OK`, `1 ERROR` (out = PluginError JSON), `2 PANIC`
(caught at the boundary), `3 INVALID_ARGUMENT`.
Operations: `1 HISTORY`, `2 LIVE_INIT`, `3 LIVE_UPDATE`.

Ownership: every `CvdBuffer` written by the library is released by calling
**that library's** `free_buffer` exactly once. Handles are opaque and
destroyed with `destroy`. No `Vec`/`String`/trait object crosses the boundary.
Panics never unwind across `extern "C"`: the SDK wraps every entry in
`catch_unwind`; a panic during `invoke` also drops that instance's live state.

## 3. Authoring (safe Rust)

```rust
use cvd_indicator_sdk::*;
struct MyInd { n: usize }
impl Indicator for MyInd {
    fn descriptor() -> Descriptor { /* id, version, context, params, outputs */ }
    fn new(p: &ParamValues) -> Result<Self, PluginError> { Ok(Self { n: p.int("n").unwrap() as usize }) }
    fn compute(&mut self, ctx: &ComputeContext, bars: &[InputBar], output_from: usize)
        -> Result<SeriesSet, PluginError> { /* columns for bars[output_from..] */ }
}
export_indicator!(MyInd);
```

Crate type `cdylib` (plus `rlib` if you want in-process unit tests). The SDK
handles parameter defaults/range checks, input monotonicity/finite checks,
live window bookkeeping and output validation. Reference: `examples/plugins/sample-indicator`.

## 4. Descriptor

```json
{ "id": "sample.channel", "version": "1.0.0", "name": "…", "description": "…", "abi": 1,
  "context": { "bars": 500, "per_tf": { "1h": 300 } },
  "params":  [ { "key": "length", "label": "Length", "type": "int", "default": 20, "min": 1, "max": 500 } ],
  "inputs":  [ "price", "aggregate_price", "delta", "aggregate_delta" ],
  "outputs": [ { "key": "sma", "label": "SMA", "pane": "price", "pane_group": null, "colors": ["#f5a623"], "type": "line" },
               { "key": "channel", "pane": "price", "type": "dual_range" },
               { "key": "delta", "pane": "separate", "pane_group": "flow", "type": "histogram" },
               { "key": "extremes", "pane": "price", "type": "segmented_lines", "layers": 2, "tail_marker": true } ],
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

## 5. Canonical input (`InputBar`)

Times are unix **seconds** of bucket open. Names reuse the established CVD bar
wire names:

| field | meaning |
|---|---|
| `time` | bucket open (s) |
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

Output (all kinds aligned to `times`, values finite or `null`):

```json
{ "times": [1700000060, …],
  "series": { "sma": {"type":"line","values":[null, 101.2]},
              "channel": {"type":"dual_range","upper":[…],"lower":[…]},
              "delta": {"type":"histogram","values":[…]},
              "extremes": {"type":"segmented_lines","layers":[[…],[…]]} } }
```

The set of series must equal the descriptor outputs exactly.

## 7. Runner frame protocol

40-byte little-endian header, then `params_len` bytes params JSON, then
`body_len` bytes body JSON:

```
"CVDP" | u16 version=1 | u16 kind | u64 request_id | u64 session | u64 generation | u32 params_len | u32 body_len
```

Kinds: `1 Hello`(host→runner, body `{"protocol":1,"abi":1}`), `2 Ready` (body =
descriptor JSON), `10 History`, `11 LiveOpen`, `12 LiveUpdate` (no params),
`13 LiveClose`, `20 Result` (body = library output bytes verbatim), `21 Error`
(public PluginError), `30 Shutdown`.

* The host encodes each request once; the runner forwards the params/body
  byte ranges to the library and the library's output bytes back without
  re-serialisation.
* Every response echoes `request_id`, `session`, `generation`; any mismatch,
  bad magic/version/kind or oversize frame is a protocol violation and the
  host kills the runner.
* `LiveUpdate` whose `generation` differs from the session's open generation is
  rejected (`stale_generation`). Hosts bump generation on source reset so stale
  results never attach to new data.
* Library writes to fd 1 are redirected to stderr by the runner; only frames
  appear on stdout. Runner stderr is captured (bounded tail) for **local**
  diagnostics only.

## 8. Limits and failures

Defaults (`protocol::Limits`): request 16 MiB, response 32 MiB, context ≤ 8192
bars, ≤ 16 384 bars per request, ≤ 512 live sessions per runner. Sized from the
canonical bar JSON (~560 B/bar): 1600-bar page + 8192 warm-up ≈ 5.5 MB.

Public error codes: `invalid_input invalid_params compute_failed plugin_panic
sequence_gap stale_generation unknown_session limit_exceeded invalid_output
protocol_error unavailable timeout crashed incompatible`. Messages are ≤ 512
bytes and never contain host paths, stderr, or raw OS errors.

| event | runner | host |
|---|---|---|
| plugin error / panic / invalid output | stays up | marks that result unavailable |
| abort / segfault / exit | dies | `crashed`, restart with backoff, sessions re-opened by replay |
| hang | killed at deadline | `timeout`, same as crash |
| bad ABI / missing symbol / bad descriptor | handshake `Error`, exits | `incompatible`, indicator marked failed |

## 9. Testing

`cargo test -p cvd-indicator-host` builds the real runner, the sample library,
the fault-injection fixture (`tests/fixtures/plugins/fault-indicator`: error,
panic, abort, segfault, hang, bad output, stdout noise, create failure) and a
wrong-ABI library, then exercises them as real processes.

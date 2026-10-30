# Browser WASM plugin ABI — `cvd-wasm` v1 (normative)

This document is the single public normative contract for CVD indicator plugins.
Plugins are standalone `wasm32-unknown-unknown` modules that a visitor imports
locally in the browser (Indicators → Import). The server never loads, stores or
executes plugins: it serves canonical base data (`/history`, `/ws`) and the two
trusted runtime modules `static/wasm-host/cvd_browser_host.wasm` (Rust policy /
validation / dispatch host) and `static/wasm-host/cvd_browser_coordinator.wasm`
(Rust data/session planner), both built from this repository's source in the
container image (`scripts/assemble-static.sh` writes `wasm-host/assets.json`
with their sha256/size). JSON request/response bodies are those of
`docs/plugin-api-v1.md`; on any conflict about module profile, resources,
lifecycle or errors, this document wins.

Authoring summary: keep `impl Indicator for T` + `export_indicator!(T)`, add
`.cargo/config.toml` `[target.wasm32-unknown-unknown] rustflags=["-C","link-arg=--max-memory=536870912"]`
and `[profile.wasm-release] inherits="release" panic="abort"`, then
`cargo build --target wasm32-unknown-unknown --profile wasm-release`.
Self-contained conformance gate: `scripts/wasm-foundation-gate.sh [OUT_DIR]`
(host-owned public fixtures only).

## 0. Base-data boundary (producer → browser)

- `/history` returns canonical renderable bars only, plus `invalid_times`:
  bucket times inside the page range that the producer dropped as
  non-renderable. A plugin input window must not be treated as contiguous
  valid data across such a time (this replaces the retired server-side
  fail-closed window guard). Fields `indicators` / `indicator_api_version`
  and the `/indicators` route no longer exist; `?indicators=` is ignored.
- `/ws` carries base frames `{rev, series, action, bar}` with `action`
  `append` | `replace_last`, plus invalidation frames
  `{rev, series, action: "invalidate", invalid_time}` (no `bar`; no
  fabricated OHLC) when a same-time version of a bucket becomes
  non-renderable. The producer evicts the earlier value from its cache; the
  browser removes the bucket from the shared cache, base series and readout,
  and the trusted coordinator refuses History/live windows spanning it
  (plugin status `unavailable` / `invalid_input_window`, values withdrawn,
  never bridged). Only a newer valid version clears it: a later-`rev` frame
  on the same connection, any frame of a newer connection, or a `/history`
  page requested after the invalidation. `rev` is the
  per-source monotonic revision (process-local; restarts on producer
  restart). Source/TF identity is the request scope;
  authoritative cumulative CVD (`cvd_usdt`, aggregate `ac`) is computed by the
  server and never recomputed by plugins.

## 1. Identity and versions
- WASM ABI version 1: export `cvd_wasm_abi_version()` returns 1 (`WASM_ABI_VERSION`).
- JSON models unchanged SDK v1 (`Descriptor` with `abi:1`, `HistoryRequest`, `LiveInit`,
  `LiveUpdate`, `Output`, `PluginError`), byte-identical to native C ABI v1.
- Target `wasm32-unknown-unknown`, standalone core module, no wasm-bindgen/JS glue.
- Authors keep `impl Indicator for T` + `export_indicator!(T)`: wasm32 emits the exports
  below; native still emits `cvd_plugin_entry_v1` (native builds are only for offline numerical tests; the server no longer loads plugins).

## 2. Module profile (bounded Rust policy check inside the plugin Worker, BEFORE `WebAssembly.compile`)
- File ≤ 16 MiB; magic `\0asm`, version 1 (not a component).
- No imports of any kind (`forbidden_import`); no start section (`forbidden_start`).
- Exactly one defined memory exported as `memory`, not shared, not memory64,
  initial ≤ 256 pages (16 MiB), **declared max REQUIRED**, ≤ 8192 pages (512 MiB) (`bad_memory`).
  The browser enforces the max on every `memory.grow`, so in-call growth is capped.
- Tables: ≤ 1, `funcref` only, **declared max REQUIRED**, ≤ 65 536, max ≥ min (`bad_table`/`resource_limit`).
- Parser counts: types ≤ 4096 (arity ≤ 64), functions ≤ 100 000, exports ≤ 64, memories ≤ 1
  (`resource_limit`). Sections 0 (custom, skipped), 1–7, 9–12; tag (13)/unknown → `unsupported_feature`.
- Exports: exactly the required set plus optional globals `__data_end`, `__heap_base`; others →
  `unexpected_export`; signatures checked (`missing_export`, `bad_signature`).
- `WebAssembly.compile` stays the structural-validity authority after this check.

### Required exports (all scalars `i32`)
| export | signature | semantics |
|---|---|---|
| `memory` | memory 0 | private linear memory of this instantiation |
| `cvd_wasm_abi_version` | `() -> i32` | returns 1 |
| `cvd_alloc` | `(len) -> ptr` | host input buffer, `1..=32 MiB`; 0 = failure |
| `cvd_dealloc` | `(ptr, len)` | release a `cvd_alloc` buffer |
| `cvd_describe` | `() -> status` | Descriptor JSON in the result slot |
| `cvd_create` | `(ptr, len) -> status` | creates the single persistent instance from params JSON; second create → 3 |
| `cvd_invoke` | `(op, ptr, len) -> status` | op 1 history, 2 live_init, 3 live_update |
| `cvd_destroy` | `() -> ()` | idempotent |
| `cvd_result_ptr` / `cvd_result_len` | `() -> i32` | last result location (plugin-owned) |

Status: 0 OK, 1 ERROR (PluginError JSON), 2 PANIC (reserved; a wasm panic is a trap), 3 INVALID_ARGUMENT.

## 3. History vs live instance semantics
Mirrors the original native runner (`indicator-runner` `FrameKind::History`: create → invoke → destroy;
`LiveOpen`/`LiveUpdate` on a persistent session):
- `op 1 History` is **stateless**: the SDK glue builds a FRESH `Instance` from the params stored
  at `cvd_create`, invokes it and drops it. It never reads or mutates the live instance.
- `op 2/3 Live*` run on the ONE persistent instance created by `cvd_create`.
- Repeated identical histories return identical bytes; History A/B/A and History interleaved with
  LiveInit/LiveUpdate equal the native runner bit-for-bit (mutable-state fixture
  `tests/fixtures/wasm/stateful-plugin`, native and browser).
- Raw (non-SDK) modules must honour the same rule.

## 4. Buffer ownership, result lifetime, checked offsets
- Host → plugin: `cvd_alloc(n)`; host checks `ptr != 0 && ptr+n ≤ memory.byteLength`; writes a fresh
  copy; calls the export; **copies the result**; only then `cvd_dealloc(ptr, n)`.
- The result slot is valid from return of `cvd_describe/cvd_create/cvd_invoke` until the next call of
  any export other than `cvd_result_ptr/len`, **including `cvd_dealloc`**. Zero-length result valid.
  A trap in `cvd_dealloc` is fatal `trap`.
- Plugin → host: `check_region(mem_len, ptr, len, max)` in u64 arithmetic; failure →
  `invalid_output_region` (fatal).
- Limits: input ≤ 32 MiB, output ≤ 32 MiB, descriptor ≤ 256 KiB, params ≤ 64 KiB, memory ≤ 512 MiB
  (declared max + post-call `memory_limit` check). Main thread caps module/params/body sizes before
  encoding or structured clone.
- Output validation (Rust): `Output::validate(descriptor, expected_times)`; expected times =
  `bars[output_from..]` (history), last bar (live_init), touched suffix of the host-tracked window
  (live_update). Plugin error JSON → `plugin_error` with `<public code>: <fixed message>`.

## 5. Live admission (host mirrors SDK `author.rs`)
Shared `check_bars`; live_init requires `descriptor.live`; window cap =
`context.bars_for(tf) + 1 + LIVE_REPLACE_SLACK`; fixed revision (`stale_revision`), exact
`seq = last+1` (`sequence_gap`), replace target must be in window, trim after ops,
`delta exceeds live window` if the earliest touched bar was trimmed. Host live state commits only on a
validated OK; host rejections / `plugin_error` leave it unchanged.

## 6. Traps, deadlines, physical cancellation, reload
- Plugins build with `panic = "abort"` (`--profile wasm-release`); native keeps `panic = "unwind"`.
  Any trap is fatal: Worker terminated, instance retired.
- Defaults (`HostLimits`): compile+validate 10 s, instantiate+describe+create 5 s, compute 5 s;
  overrides must be known keys, finite, in (0, 60000] (`bad_limits`). Expiry → `terminate()`,
  waiters rejected `deadline`, generation retired.
- **Cancellation is physical.** `supersede(instance, scopeKey, revision)`:
  - if it drops an outstanding request (by construction the one executing in that Worker), the
    promise is rejected `superseded` and that exact Worker incarnation is terminated and retired
    immediately — no later request acts as watchdog, no old live state survives;
  - if nothing is in flight it only advances the fence; the instance is kept.
- **Reload contract for B:** after `superseded`, `deadline`, `trap`, any fatal error or `disable`,
  `compute(instance)` rejects `stale`; B must `load(instance, bytes, params)` again (new generation)
  and, for live, resend LiveInit before LiveUpdate.

## 7. Lifecycle, identity, ownership
- One app-owned Worker per instance (and a one-shot Worker per `validateModule`), empty import object,
  separate `WebAssembly.Memory` each; no plugin JS evaluated; untrusted parsing never on the UI thread.
- At most one request in flight per instance (`busy`); fence quota `MAX_PENDING = 64`.
- Every timer/onmessage/onerror is bound to the exact Worker incarnation; late callbacks of a replaced
  Worker are no-ops.
- **Every load failure after spawn** (admission/quota rejection before any timer, Worker constructor
  failure, postMessage failure, worker error, deadline, `plugin_error` on create) retires exactly that
  incarnation, leaving zero instances/waiters/timers and no fence `live`/`pending` entry; other
  instances are unaffected.
- Responses must carry `id, instance, generation, type, scopeKey (string), revision (int)`; expected
  kind per op: load → `loaded|error` (scope `__load`, rev 0, op 0); validate → `validated|error`
  (scope `__validate`); compute → `result|error`. The bridge checks the originating incarnation before
  the Rust fence consumes the entry; bad frames are dropped, counted and never throw.

### Worker messages
main→worker: `{type:"load", id, instance, generation, scopeKey, revision, op, hostWasm, pluginWasm, params}`,
`{type:"validate", …ids, hostWasm, pluginWasm}`, `{type:"compute", …ids, op:1|2|3, body}`.
worker→main: `{type:"loaded", …ids, descriptor}` | `{type:"validated", …ids, report}` |
`{type:"result", …ids, output}` | `{type:"error", …ids, code, message, fatal}` (fatal ⇒ worker closes; main retires).
Worker nonfatal compute codes: `plugin_error, invalid_input, sequence_gap, stale_revision, input_too_large`.

## 8. Host APIs
Rust (`cvd-browser-host`): `module_check::validate_module`, `contract::check_region`,
`contract::PluginSession` (`accept_abi`, `accept_descriptor`, `accept_create`, `begin`, `accept_result`,
`poison`), `fence::Controller` (`start`, `issue`, `accept`, `retire`, `supersede`, `cancel`,
`live_len`, `pending_len`).
wasm32 FFI (status 0/1, JSON slot `host_result_ptr/len`): `host_alloc, host_free, host_limits,
host_validate_module, host_session_reset, host_accept_abi, host_check_region, host_accept_descriptor,
host_accept_create, host_begin, host_accept_output, host_poison, host_fence`; fence cmds
`start → {generation}`, `retire`, `issue → {id, generation}`, `supersede → {dropped:[ids]}`, `cancel`,
`accept → {verdict:"accepted", op} | {verdict:"rejected", reason}`, `stats → {pending, live, rejected}`.
Trusted JS bridge (`WasmPluginHost` in `static/wasm-host/host-bridge.js`): `WasmPluginHost.create(hostWasm, {limits, workerUrl})`;
`validateModule(bytes) → Promise<ModuleReport>`; `load(instance, bytes, params) → {generation, descriptor}`;
`compute(instance, scopeKey, revision, op, body) → validated Output JSON text`;
`supersede(instance, scopeKey, revision) → cancelled count`; `disable(instance)`;
`stats() → {pending, live, rejected, dropped, running, waiters, timers}`.

Error codes: `module_too_large, bad_magic, bad_module, malformed_module, forbidden_import, forbidden_start,
bad_memory, bad_table, resource_limit, unsupported_feature, missing_export, bad_signature,
unexpected_export, abi_mismatch, bad_descriptor, invalid_output_region, invalid_output, plugin_error,
invalid_input, input_too_large, sequence_gap, stale_revision, memory_limit, trap, deadline, poisoned,
stale, busy, superseded, disabled, replaced, worker_failed, post_failed, bad_limits, bad_command, lifecycle`.

## 9. Build and gate (one command)
```
CVD_TARGET_DIR=<target> scripts/wasm-foundation-gate.sh <OUT_DIR>
```
Builds `sample-indicator`, `stateful-indicator`, `cvd-browser-host` (wasm-release; `.cargo/config.toml`
`--max-memory=536870912`) and the hostile fixture with a 4 MiB max; generates fixtures + native C-ABI
oracles (`native_oracle -- --fixtures`: request, history oracle, live script, stateful script, hostile
modules); runs `cargo test --locked --workspace`; runs the real Chromium Worker smoke
(`tests/wasm-host-browser.mjs`, loopback only); writes artifact/fixture hashes; prints `GATE PASS`.
Plugin authors (C/D): copy `[profile.wasm-release]` (`panic="abort"`) and the max-memory flag;
`crate-type` includes `cdylib`; no other source change.

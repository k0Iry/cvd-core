# Frontend indicator host

Indicators run locally as imported `.wasm` files, one Worker per enabled
plugin. Bar plugins draw on the chart; `order_book` plugins draw in the Book
panel. Files stay in the browser; there are no plugin accounts or special
indicator ids. The server serves base market data, not indicator results.
See the [WASM contract](plugin-wasm-v1.md) for plugin authoring and ABI limits.

## Responsibilities

| Component | Responsibility |
|---|---|
| `cvd-server` | Ingestion, recovery, persistence and canonical bars, including cumulative CVD. |
| `crates/browser-host` | Module admission, ABI validation, limits and result fencing. |
| `crates/browser-coordinator/src/sched.rs` | Bars: scheduling, history/live precedence, warm-up, readiness, scope/revision checks and accepted values. |
| `crates/browser-coordinator/src/book.rs` | Book: one compute in flight per plugin, latest-frame planning, result fencing and staleness. |
| `static/wasm-host/host-bridge.js` | Worker calls (op 1–3 bars, op 4 book), coordinator binding and IndexedDB storage; descriptor/parameter approval is delegated to Rust. |
| `static/wasm-host/plugin-worker.js` | Validate before compile, instantiate and execute one request at a time. |
| `static/js/app.js` | Controls, preferences, base-data transport and application of coordinator view diffs. |
| `static/js/chart.js` | Generic bar rendering by output kind, panes, readouts and display-only time labels. |
| `static/js/book-panel.js` | Moves `/book` frames to the coordinator, runs planned computes and draws admitted `price_levels` rows. |

No indicator algorithm or indicator-id dispatch lives in the chart. The public
CVD plugin projects the server's cumulative CVD; it does not re-accumulate it.

## Lifecycle

- **Import:** check file/name/size, hash and validate in a bounded Worker.
  A new plugin starts disabled. Identical bytes are a no-op; replacing a plugin
  is atomic and keeps enable intent and compatible parameters.
- **Enable / disable / remove:** enable starts computation; disable terminates
  the Worker and clears results but retains the file and settings; remove also
  deletes the stored package. No server restart is needed.
- **Bar data:** the Rust coordinator selects stateless History or bounded
  LiveInit/LiveUpdate work and accepts only current results. Source/timeframe/mode
  changes withdraw old-scope values. Missing warm-up uses the shared history path.
- **Book data:** `/book` frames (exchange/market/symbol, no timeframe) are
  planned one at a time, newest first. Results are fenced on source switch,
  reconnect or resync; non-live, silent or expired books withdraw rows.
- **Faults:** stop only the affected plugin and show its status. No server/native
  fallback or automatic restart hides failures.
- **Bar lifecycle:** hidden pages cancel in-flight bar-plugin calls and pause
  computation/painting; returning resumes from the latest demand. This does not
  apply to the Book path. Unchanged bar values cause no chart writes; tail-only
  updates use the incremental path.

The indicator popup uses native checkboxes, preserves focus during updates and
stays inside the viewport. Tab/Shift+Tab navigate it; Escape closes it and returns
focus to the trigger. Toggling an indicator does not reload market data or move
the viewport.

## Rendering rules

* Price pane is host-owned (candles + hidden whitespace **backbone** series
  holding every cached timestamp). Backbone never belongs to a plugin; plugin
  data can never extend the time axis.
* `pane: "price"` overlays price; `pane: "separate"` → one pane per
  (indicator, `pane_group` ?? output key), in discovery order.
* Kinds: `line` → 1 native line; `histogram` → 1 native histogram + zero
  line; `dual_range` → separate pane: dual-range histogram around 0 (`upper`,
  `lower` signed); price pane: two band-edge lines; `segmented_lines` → per
  layer ≤ 2 series (one custom history series + one native tail carrying the
  last-value label when `tail_marker`). No per-gap series.
* Two or more bar-type outputs (histogram/dual_range) in one separate pane are
  alternatives → generated mode `<select>`; lines always draw.
* `readout_only` lines are never drawn; they appear in the pane readout (Flow
  `Total` in both modes).
* `colors`: line [stroke]; histogram [up, down]; dual_range [upper, lower];
  segmented one per layer. `widths`: empty = native default (segmented 1),
  one value = whole output, one per layer for segmented.
* Missing value (absent, `null`, non-finite) = whitespace; true zero stays
  zero. Faulted/unavailable/disabled outputs are cleared and labelled.

## Base-data fencing

Server indicator frames are ignored. Book frames are fenced by the Book
coordinator (above). Base-bar `rev` values are fenced within a
WebSocket connection epoch; reconnect starts a new epoch and reload clears the
fence. These transport epochs are separate from plugin request revision/sequence
rules. See [source identities and units](instruments.md) and
[source selection](usdc-ui.md).

## Local checks

Run from the repository root with Rust and Node 22. The browser check also needs
Chromium (`CVD_CHROMIUM` can select its path):

```sh
bash scripts/node-unit-gate.sh ../cvd-node-check
bash scripts/core-app-gate.sh ../cvd-browser-check 1
```

The first builds the coordinator and runs browser-free unit contracts. The second
builds the host/coordinator and core-owned **TEST-ONLY** plugins, then exercises the
real app and Worker through the file input. It does not validate an author's own
plugin; use the [authoring guide](https://github.com/k0Iry/cvd-indicators/blob/master/docs/authoring.md#4-test)
for that.

Push/PR CI is browser-free. Real-browser checks remain available locally and on
manual `workflow_dispatch`; these commands are not needed to deploy or use CVD.

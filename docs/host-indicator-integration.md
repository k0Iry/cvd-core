# Host indicator integration (HTTP / WebSocket) — envelope v1

Companion to `plugin-api-v1.md`. This is the authoritative browser contract;
schemas are the server types named below. A real serialized example (server
types + runner + public sample/fault libraries) is
`docs/fixtures/web-envelope-v1.json`, regenerated with:

    CVD_EXPORT_WEB_FIXTURE=$PWD/docs/fixtures/web-envelope-v1.json \
      cargo test --offline -p cvd-server export_web_envelope_fixture

(`generation`/`session` values in it are run-specific.) The host is generic:
it never branches on indicator id; rendering is driven by the SDK descriptor
(`type`, `pane`, `pane_group`, `colors`, `widths`, `readout_only`,
`tail_marker`).

## Configuration

`CVD_CONFIG=/path/cvd.toml` (see `config/cvd.example.toml`). Without it the
host starts with no indicators and still serves price, base flow fields and
source quality. Libraries come only from operator-listed absolute paths;
clients select installed **ids**, never paths/URLs. Each library runs in its
own `cvd-indicator-runner` process (empty environment, cwd `/`, killed with
the host on Linux). This contains crashes/hangs; it is not a security sandbox.

## `GET /indicators` (`indicators::IndicatorInfo`)

```json
{ "api_version": 1,
  "indicators": [
    { "id": "sample.channel", "status": "ready", "modes": ["source","aggregate"],
      "version": "1.0.0", "descriptor": { "…SDK v1 descriptor…": 0 }, "error": null, "restarts": 0 },
    { "id": "x.y", "status": "unavailable", "modes": ["source"], "version": null, "descriptor": null,
      "error": {"code":"incompatible","message":"indicator library is incompatible"}, "restarts": 3 } ] }
```

`status` ∈ `starting | ready | unavailable` (unavailable covers failed,
incompatible, restarting and operator-disabled). An indicator can become
`ready` after initial discovery: clients refresh discovery (bounded) at
startup and on `indicator_status` / `indicator_reset` for ids without a
descriptor. Also embedded as `indicators` in `/meta`; `/health` carries a
summary that never affects ingestion health.

## `GET /history?…&indicators=<id,id>` (`models::HistoryPayload`)

`indicators` absent → all configured; empty → none; unknown ids ignored.
Canonical base bars are unchanged (no `lt/lb/alt/alb` or other per-bar
indicator expansion). Added fields:

```json
"indicator_api_version": 1,
"indicators": {
  "sample.channel": { "version": "1.0.0", "modes": {
    "source":    { "status": "ok", "output": { "times": [], "series": { "sma": {"type":"line","values":[]} } } },
    "aggregate": { "status": "unavailable", "error": { "code": "timeout", "message": "indicator timed out" } } } } }
```

(`indicators::IndicatorHistory` / `ModeResult`; `output` is SDK `Output`,
columnar.) `output.times` equals the page's `bars[].time` exactly: canonical
timestamps (final second of each bucket), never floored or shifted. Warm-up
rows declared by descriptor `context` are fetched, used and trimmed. Pages are
stateless and may be fetched in any order.

## WebSocket `/ws`

Base frames (`models::WsPayload`) stay non-blocking and are never delayed by
indicators: `{rev, series, action, bar}`; `rev` is a per-source monotonic
input revision (additive in v1). Quality enrichment is unchanged.

Indicator frames are separate (`live_indicators::IndicatorFrame`, tagged by
`type`, all with `api_version: 1`):

| `type` | fields |
|---|---|
| `indicator` | `id, version, mode, generation, session, seq, as_of_rev, output{times, series}` |
| `indicator_status` | `id, mode, generation, session, status: ok/unavailable, error?` |
| `indicator_reset` | `generation` |

* Fencing: accept frames only for the current connection scope
  (exchange/market/symbol/tf) and latest `generation`; per (`id`, `mode`) the
  highest `session` wins; within a session `seq` strictly increases. A runner
  restart or session reopen yields a new `session` starting at `seq` 0,
  possibly under the same `generation`. Older session/seq never overwrites
  newer.
* Output covers the first bar touched by a delta through the newest bar;
  replace points with equal `time`. Output never extends the host time axis:
  frames referencing a `time` without a base bar yet are held (bounded) and
  merged when that base bar arrives.
* Revision correlation: `as_of_rev` = newest base-frame `rev` of this source
  included in the computation (conservative on session open). A point is
  current for the displayed base bar only if `as_of_rev >= rev` of that base
  frame; otherwise treat it as pending, not as computed from newest input.
* `indicator_status` `unavailable`: clear or mark that id/mode unavailable
  (never stale or zero). `indicator_reset` (hub overflow/resync or viewer
  lag): drop live indicator state and refetch `/history`.
* All viewers of a scope share one hub; ingestion hands bars over without
  awaiting; failed sessions reopen by replaying a bounded window (declared
  context + 1 bars) from host cache — never full history per tick.

## Public errors

`error` is always `{code, message}` with a fixed message per code
(`indicators::sanitize`). Library-provided text, runner stderr, filesystem
paths, panic payloads and OS errors are logged locally (bounded) only.

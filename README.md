# cvd-core

Public market-data host, SDK, renderer and browser-local WASM plugin host.

Clean-history implementation: existing CVD Git history is not imported.

The server collects exchange trades and serves base market data plus the web
app. **Indicators are `.wasm` modules each visitor imports in the browser**; the
server never loads, stores or runs indicator plugins. Public indicator builds are
released in [cvd-indicators](https://github.com/k0Iry/cvd-indicators/releases);
custom/private `.wasm` files are imported the same way from the user's own disk.

| path | contents |
|---|---|
| `crates/cvd-server` | host binary `cvd`: ingestion (Binance, Coinbase spot, Coinbase USDC perps), recovery, PostgreSQL persistence + SQLite outbox, aggregation, source quality, HTTP/WS, static web app |
| `crates/indicator-sdk` | SDK types for indicator authors |
| `crates/browser-host` | trusted Rust→WASM plugin policy/validation/dispatch host (`static/wasm-host/cvd_browser_host.wasm`) |
| `crates/browser-coordinator` | trusted Rust→WASM data/session planner (`static/wasm-host/cvd_browser_coordinator.wasm`) |
| `tests/fixtures` | public protocol/WASM test fixtures |
| `docs/plugin-wasm-v1.md` | normative browser WASM plugin ABI + base-data boundary |
| `docs/plugin-api-v1.md` | HISTORICAL retired native C-ABI/runner contract (not used by current releases) |

## Deploy / upgrade with Docker Compose

**[English deployment guide](docs/deployment.md) · [中文部署指南](docs/deployment.zh-CN.md)**

The Docker build compiles the server and the two trusted runtime WASM modules;
the host needs only Git + Docker Compose v2. No indicator repository, `.so`
files or plugin mounts on the server.

- **Existing deployment:** keep your own Compose files/project name, `.env`,
  `artifacts/cvd.toml`, PostgreSQL volume and outbox mount; back up to a new directory
  (guide step 2), then
  `git pull --ff-only`, `docker compose build cvd`,
  `docker compose up -d --no-deps --no-build cvd` (with your usual `-f`/`-p`).
  Legacy `[indicators]` config is ignored with a warning.
- **Fresh install:** `.env` password, `artifacts/cvd.toml` from
  [`config/cvd.public.toml`](config/cvd.public.toml), then
  `docker compose build cvd` and `docker compose up -d --no-build`.
- **Users:** hard-refresh the page, then Indicators → Import a `.wasm` and tick
  it. Install/toggle/parameters are browser-local; no server restart.
- `/health`, `/meta`, `/wasm-host/assets.json` are the useful checks;
  `/indicators` is retired (404).

## Market sources

BTC and ETH are supported on Binance USDT/USDC spot, USDT/USDC linear perpetuals,
COIN-M inverse perpetuals, Coinbase USD spot and Coinbase USDC perpetuals.
See [instrument identities and units](docs/instruments.md) and
[USDC source selection](docs/usdc-ui.md) for exact keys and UI behavior.

New Binance USDC sources have independent history, cursors and CVD seeds; existing
USDT/Coinbase rows are neither relabelled nor backfilled. Aggregate turnover is
nominal USD/USDT/USDC without FX conversion. This addition changes no database
schema: retain existing data/outbox and rebuild only the core application to
upgrade. Longer-context indicators may need new-source warm-up.

## Build

    cargo build --release -p cvd-server   # server only; trusted wasm: see Dockerfile / scripts/assemble-static.sh

New plugin authors: start from the public
[`examples/range`](https://github.com/k0Iry/cvd-indicators/tree/master/examples/range)
example and the
[authoring guide](https://github.com/k0Iry/cvd-indicators/blob/master/docs/authoring.md)
in the cvd-indicators repository; build a `.wasm` there and import it in the browser.

## Run

| env | meaning |
|---|---|
| `DATABASE_URL` | PostgreSQL (required) |
| `CVD_CONFIG` | optional TOML (`[server]`: HTTP address, static dir; legacy `[indicators]` is ignored with a warning) |
| `CVD_OUTBOX_PATH` | SQLite outbox path (default `data/outbox.sqlite`) |
| `LIVE_ACCESS_KEY` | optional realtime access key |

## Test

    cargo test --locked --workspace  # add --offline after dependencies are cached

No network or database tests run by default (they are `#[ignore]` opt-ins).
Opt-in real PostgreSQL tests use a disposable database only, via Unix socket:

    CVD_RETIREMENT_TEST_URL='postgresql://<user>@localhost:<port>/cvd_retirement_test?host=<socket-dir>' \
      cargo test --locked --offline -p cvd-server -- --ignored --test-threads=1 postgres_retirement postgres_usdc_rows

Frontend/browser gates need Node and Chromium (not needed for Docker deployment):
`scripts/node-unit-gate.sh <OUT_DIR>`, `scripts/wasm-foundation-gate.sh`,
`scripts/core-app-gate.sh <OUT> [reps]` (assembled app, 12 real-Chromium scenarios).
`tests/wasm-oracle-client.mjs` is the test-only numerical oracle for
`tests/browser-interaction.mjs`.

Format only owned packages (`cargo fmt -p <pkg>`), never `cargo fmt --all` from a
repository that has path dependencies on this one.

## Indicators (browser-local WASM)

Indicators run in the visitor's browser as imported `.wasm` modules; the server
never loads plugins. Normative ABI/resource/lifecycle contract:
[`docs/plugin-wasm-v1.md`](docs/plugin-wasm-v1.md). Imported modules live in
that browser's IndexedDB per origin and are never uploaded. Deployment/upgrade
steps: [`docs/deployment.md`](docs/deployment.md).

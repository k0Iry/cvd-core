# cvd-core

Public market-data host, SDK, renderer and isolated dynamic-library runner.

Clean-history implementation: existing CVD Git history is not imported.

The public SDK/protocol lives here. Separately built public libraries are in
[cvd-indicators](https://github.com/k0Iry/cvd-indicators). Private libraries can
be mounted on the operator's server; their source and binaries are never part
of this repository or public image. Browser clients receive values only.

| path | contents |
|---|---|
| `crates/cvd-server` | host binary `cvd`: ingestion (Binance, Coinbase spot, Coinbase USDC perps), recovery, persistence/outbox, aggregation, source quality, HTTP/WS, indicator supervision |
| `crates/indicator-sdk` | ABI v1 / protocol v1 SDK for indicator authors (`Indicator` trait + `export_indicator!`) |
| `crates/indicator-runner` | `cvd-indicator-runner`: loads one operator-configured library in its own process |
| `crates/indicator-host` | async runner client used by the host |
| `examples/plugins/sample-indicator` | public reference library |
| `tests/fixtures/plugins` | fault-injection, bad-ABI and zero-context test libraries |
| `docs/plugin-api-v1.md` | normative library contract |
| `docs/host-indicator-integration.md` | HTTP/WS indicator contract |

## Deploy with Docker Compose

**[Start here: English deployment guide](docs/deployment.md) · [中文部署指南](docs/deployment.zh-CN.md)**

The public quick start builds core and the public CVD/Flow plugins locally, then
starts PostgreSQL and the application with [`compose.yaml`](compose.yaml).
It includes persistent database/outbox volumes, a localhost-only port, plugin
configuration and existing-deployment upgrade instructions. No private repository
or CI run is required; private/custom libraries are optional.

- Copy [`.env.example`](.env.example) to `.env` and choose a strong password.
- Use [`config/cvd.public.toml`](config/cvd.public.toml) for public CVD/Flow.
- Adding/disabling plugins needs an application restart, **not** a core rebuild;
  hot reload is not implemented.
- Existing deployments must retain their database and outbox rather than blindly
  adopt the fresh-install volumes. The guide shows the distinction.

## Build

    cargo build --release -p cvd-server -p cvd-indicator-runner   # host + runner
    cargo build --release -p sample-indicator                      # optional example library

Indicator libraries (e.g. the separate cvd-indicators repository) are built
against `crates/indicator-sdk` and deployed as `.so` files listed in the
operator config. The core builds and runs without any of them.

## Run

| env | meaning |
|---|---|
| `DATABASE_URL` | PostgreSQL (required) |
| `CVD_CONFIG` | optional TOML (`config/cvd.example.toml`): HTTP address, static dir, runner path, indicator libraries |
| `CVD_OUTBOX_PATH` | SQLite outbox path (default `data/outbox.sqlite`) |
| `LIVE_ACCESS_KEY` | optional realtime access key |

Without indicator libraries the host still serves price, base flow fields and
source quality.

## Test

    cargo test --locked --workspace  # add --offline after dependencies are cached

Builds the runner and fixture libraries and exercises real runner processes
(errors, panics, abort, segfault, hang, bad ABI) plus host integration. No
network or database tests run by default (they are `#[ignore]` opt-ins).
Optional: `CVD_PUBLIC_INDICATOR_DIR=<dir of lib*.so>` smoke-loads external
public libraries; `CVD_EXPORT_WEB_FIXTURE=<path>` regenerates
`docs/fixtures/web-envelope-v1.json`.

Opt-in real PostgreSQL tests (disposable database only, Unix socket; each
asserts the exact database name and `inet_server_addr() IS NULL`, then uses a
connection-local `TEMP bars` table under `search_path = pg_temp`):

    CVD_PLUGIN_PG_TEST_URL='postgresql://<user>@localhost:<port>/cvd_plugin_test?host=<socket-dir>' \
    CVD_RETIREMENT_TEST_URL='postgresql://<user>@localhost:<port>/cvd_retirement_test?host=<socket-dir>' \
      cargo test --offline -p cvd-server -- --ignored --test-threads=1 pg_plugin postgres_retirement

Format only owned packages
(`cargo fmt -p <pkg>`), never `cargo fmt --all` from a repository that has
path dependencies on this one.

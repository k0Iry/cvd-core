# cvd-core

A self-hosted web app for live BTC/ETH market data (Binance and Coinbase) with
indicators that run in your browser.

- The **server** collects exchange trades, stores them in PostgreSQL and serves
  the web page.
- **Indicators** (CVD, Flow, or your own) are `.wasm` files you import in the
  browser. No server-side indicator plugin setup is needed.

## What you need

A Linux server with **Git** and **Docker** (Compose v2). The Docker build
compiles everything else; you don't need Rust, Node or a browser on the server.

## Deploy or upgrade

Follow the short guide — **[English](docs/deployment.md) · [简体中文](docs/deployment.zh-CN.md)**.
It covers upgrading an existing installation, a first install, and checking that
the app is running.

## Use indicators

1. Download `cvd_indicator_cvd.wasm` and/or `cvd_indicator_flow.wasm` from the
   [cvd-indicators v0.1.0 release](https://github.com/k0Iry/cvd-indicators/releases/tag/v0.1.0).
   Your own custom `.wasm` files work the same way.
2. Open the app, click **Indicators → Import**, choose the file, then enable it.

Your browser remembers imported indicators for this site. If you clear site
data or switch browser, import them again. No server restart is ever needed.

## Supported markets

BTC and ETH on Binance spot (USDT/USDC), Binance perpetuals (USDT/USDC linear
and COIN-M), Coinbase USD spot and Coinbase USDC perpetuals. Details:
[instruments](docs/instruments.md).

## For developers

- Write your own indicator: see the
  [authoring guide](https://github.com/k0Iry/cvd-indicators/blob/master/docs/authoring.md)
  and the [plugin format reference](docs/plugin-wasm-v1.md).
- License: Apache-2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE).

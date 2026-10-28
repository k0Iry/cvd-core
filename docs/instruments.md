# Market instrument identities and units

The API's `exchange`, `market`, and `symbol` form a stable source key. `symbol` is BTC or ETH. Venue-native products are not interchangeable; they have separate execution IDs, recovery cursors, CVD seeds and persisted bars. `/meta` retains its existing fields and adds `instruments[]`, each with `exchange`, `market`, `symbol`, `native_symbol`, `category`, `contract_kind`, `quote_asset`, and `settlement_asset`. The server's source table drives both supported selections and these descriptors.

| Exchange | Market key | Native BTC / ETH products | Category | Contract | Quote | Settlement |
|---|---|---|---|---|---|---|
| Binance | `spot` | BTCUSDT / ETHUSDT | spot | null | USDT | null |
| Binance | `spot_usdc` | BTCUSDC / ETHUSDC | spot | null | USDC | null |
| Binance | `usdm` | BTCUSDT / ETHUSDT | contract | linear | USDT | USDT |
| Binance | `usdm_usdc` | BTCUSDC / ETHUSDC | contract | linear | USDC | USDC |
| Binance | `coinm` | BTCUSD_PERP / ETHUSD_PERP | contract | inverse | USD | BTC / ETH |
| Coinbase | `spot` | BTC-USD / ETH-USD | spot | null | USD | null |
| Coinbase | `usdc` | BTC_USDC-PERPETUAL / ETH_USDC-PERPETUAL | contract | linear | USDC | USDC |

`coinbase/usdc` is the established Coinbase perpetual source, **not** Binance USDC. The retired `coinbase/usdm`, Bybit, unknown combinations and symbols outside BTC/ETH are not selectable. Binance USDC sources begin with their own history at first collection; neither existing USDT history nor Coinbase history is backfilled or relabelled. Empty new-source history is empty, not zero-volume coverage. `spot_usdc` retains the `spot-v1` series; `usdm_usdc` retains the `contracts-drb-v1` series. Existing keys, bar schema, persistence primary key, indicator SDK and wire field names are unchanged.

For Binance spot and USD-margined linear trades, Binance `q` is quantity in the base asset (BTC or ETH), while quote turnover is `price * q` in USDT or USDC. COIN-M `q` is a contract count: quote turnover is `q * contractSize` USD and base quantity is that result divided by price; its settlement is the base asset. The existing Coinbase sources retain their venue-specific trade parsing. Legacy bar fields ending in `_usdt` remain for wire compatibility: interpret these as each source's **nominal quote turnover**, not proof of actual USDT settlement. Cross-source aggregate nominal USD/USDT/USDC values are summed without FX conversion; they are not exact USD-converted amounts. Source quality marks missing contributors and aggregate coverage remains unverified.

Binance source startup validates official exchange metadata before opening each spot/linear WebSocket; if a symbol is unavailable, not trading, has a different base/quote, or the perpetual margin/contract fields disagree, that market is rejected and retried rather than being routed to another quote. COIN-M keeps its existing positive `contractSize` validation. Recovery uses the same native product and venue REST endpoint as the stream and never falls back to USDT.

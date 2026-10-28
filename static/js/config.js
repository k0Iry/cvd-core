export const CONFIG = {
    RENDER_WINDOW_SIZE: 3200,
    RENDER_WINDOW_BUFFER: 512,
    // Display page size only. Indicator warm-up/context is declared by each
    // plugin and satisfied server-side by the host; the browser never computes
    // indicator values and needs no algorithm-specific left context.
    HISTORY_PAGE_SIZE: 1600,
    // Rebuilding every segmented-lines output touches a lot of chart state.
    // Keep realtime candles/line/histogram outputs instant, but throttle full
    // segmented refreshes.
    SEGMENTED_REALTIME_REFRESH_MS: 2000,
    EDGE_TRIGGER: 30,
    FETCH_COOLDOWN: 300,
    DELAY_POLL_MS: 30000,
}

export const THEMES = {
    day: {
        bg: "#f6f8fb",
        panel: "#ffffff",
        border: "rgba(0,0,0,0.08)",
        text: "#1d2430",
        muted: "#6b768a",
        accent: "#2f80ed",
        accent2: "#f2994a",
        chartBg: "#f8f9fc",
        grid: "#e4e8ee",
    },
    night: {
        bg: "#0f1115",
        panel: "rgba(255,255,255,0.04)",
        border: "rgba(255,255,255,0.08)",
        text: "#e8ecf2",
        muted: "#8b94a7",
        accent: "#56ccf2",
        accent2: "#f2c94c",
        chartBg: "#11161f",
        grid: "#1f2530",
    },
}

export const FALLBACK_EXCHANGE_META = {
    binance: {
        markets: ["spot", "usdm", "coinm"],
        symbols: ["BTC", "ETH"],
    },
    coinbase: {
        markets: ["spot", "usdc"],
        symbols: ["BTC", "ETH"],
    },
}

// Stable source identities. Metadata must match the complete descriptor before
// a new market is offered; legacy fallback deliberately contains no new USDC.
const source = (exchange, market, symbol) => {
    const category = market === "spot" || market === "spot_usdc" ? "spot" : "contract"
    const quote_asset = exchange === "coinbase" ? (market === "spot" ? "USD" : "USDC")
        : market === "coinm" ? "USD" : market.endsWith("_usdc") ? "USDC" : "USDT"
    return { exchange, market, symbol,
        native_symbol: exchange === "coinbase" ? (market === "spot" ? `${symbol}-USD` : `${symbol}_USDC-PERPETUAL`)
            : market === "coinm" ? `${symbol}USD_PERP` : `${symbol}${quote_asset}`,
        category, contract_kind: category === "spot" ? null : market === "coinm" ? "inverse" : "linear",
        quote_asset, settlement_asset: category === "spot" ? null : market === "coinm" ? symbol : quote_asset }
}
export const SOURCE_INSTRUMENTS = Object.fromEntries(
    [["binance", ["spot", "spot_usdc", "usdm", "usdm_usdc", "coinm"]], ["coinbase", ["spot", "usdc"]]]
        .flatMap(([exchange, markets]) => markets.flatMap(market => ["BTC", "ETH"].map(symbol => {
            const descriptor = source(exchange, market, symbol)
            return [`${exchange}:${market}:${symbol}`, descriptor]
        }))))
export const sourceKey = (exchange, market, symbol) => `${exchange}:${market}:${symbol}`
export const sourceSeries = market => market === "spot" || market === "spot_usdc" ? "spot-v1" : "contracts-drb-v1"
export const legacySource = descriptor => descriptor.market !== "spot_usdc" && descriptor.market !== "usdm_usdc"
export function validInstrument(item) {
    if (!item || typeof item !== "object") return false
    const expected = SOURCE_INSTRUMENTS[sourceKey(item.exchange, item.market, item.symbol)]
    return !!expected && Object.keys(expected).every(field => item[field] === expected[field])
}
export const STORAGE_KEY = "cvd_prefs_v1"
export const DEFAULT_TITLE = "CVD"

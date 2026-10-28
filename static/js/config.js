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

export const STORAGE_KEY = "cvd_prefs_v1"
export const DEFAULT_TITLE = "CVD"

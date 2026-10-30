// Display-only bar labels. Canonical Bar.time is the inclusive bucket END
// second ((id+1)*tf-1); charts label a bar by its interval START instead.
// Stored/series times are never changed: only LWC tick/crosshair text.
// Convention: UTC, matching LWC's default for UTCTimestamp values.
export const TF_SECONDS = Object.freeze({ "1m": 60, "5m": 300, "15m": 900, "30m": 1800, "1h": 3600 })

export function barOpenTime(time, tf) {
    const step = TF_SECONDS[tf]
    if (!step || typeof time !== "number" || !Number.isFinite(time)) return time
    return Math.floor(time / step) * step
}

const pad = n => String(n).padStart(2, "0")
const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]

function hm(sec, seconds) {
    const d = new Date(sec * 1000)
    const base = `${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}`
    return seconds ? `${base}:${pad(d.getUTCSeconds())}` : base
}

// getTf() is read on every call, so timeframe switches never use a stale tf.
// Date-level tick types (Year/Month/DayOfMonth) return null -> LWC default.
export function createTimeLabelFormatters(getTf) {
    return {
        tickMarkFormatter(time, tickMarkType) {
            if (typeof time !== "number") return null
            if (tickMarkType === 3) return hm(barOpenTime(time, getTf()), false)
            if (tickMarkType === 4) return hm(barOpenTime(time, getTf()), true)
            return null
        },
        timeFormatter(time) {
            if (typeof time !== "number") return String(time)
            const open = barOpenTime(time, getTf())
            const d = new Date(open * 1000)
            return `${pad(d.getUTCDate())} ${MONTHS[d.getUTCMonth()]} '${pad(d.getUTCFullYear() % 100)}   ${hm(open, false)}`
        },
    }
}

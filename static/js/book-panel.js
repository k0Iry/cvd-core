// Generic live price-level panel for `order_book` plugins (OP_BOOK = 4).
//
// Thin adapter: the Rust server owns the exchange book (/book WebSocket,
// identity = exchange/market/symbol, no timeframe); the Rust browser host
// (cvd_browser_host.wasm via WasmPluginHost) validates every book input and
// every plugin result structurally; plugins run in their own Workers. Book
// planning, completion admission and result freshness are decided by the
// existing Rust coordinator (crates/browser-coordinator/src/book.rs); this file
// only moves frames to it, runs the computes it plans, and draws the views it
// admits. Rows are the plugin's derived view of the supplied depth. Non-live
// frames, disconnects, a silent feed and expired results withdraw rows.

export const BOOK_OP = 4
export const BOOK_STALE_MS = 5000
const VIEW_TICK_MS = 500
const INSTANCE_BASE = 2 ** 29
const SUPPORTED = new Set(["binance/usdm/BTC", "binance/usdm/ETH"])

export function isBookDescriptor(d) {
    return Array.isArray(d?.inputs) && d.inputs.includes("order_book")
}

export function bookSupported(src) {
    return !!src && SUPPORTED.has(`${src.exchange}/${src.market}/${src.symbol}`)
}

const fmt = (v, d) => Number.isFinite(v) ? v.toLocaleString("en-US", { minimumFractionDigits: d, maximumFractionDigits: d }) : "–"
const qtyFmt = v => v === 0 ? "" : v >= 1000 ? fmt(v, 0) : v >= 1 ? fmt(v, 3) : fmt(v, 4)
function priceDigits(rows) {
    let d = 0
    for (const r of rows) { const s = String(r.price); const i = s.indexOf("."); if (i >= 0) d = Math.max(d, Math.min(8, s.length - i - 1)) }
    return d
}

export function createBookPanel({ host, coord, container, wsUrl, WebSocketImpl = globalThis.WebSocket, now = () => Date.now(), timers = globalThis, onChange = null }) {
    const plugins = new Map() // id -> { id, name, loadKey, spec, instance, inflight: Map(token -> frame), shown: { token, frame, out } | null }
    let source = null
    let epoch = 0
    let ws = null
    let reconnectTimer = null
    let staleTimer = null
    let viewTimer = null
    let attempt = 0
    let status = { state: "off", reason: null }
    let lastFrame = null
    let nextInstance = INSTANCE_BASE
    let views = new Map()
    const counters = { frames: 0, computes: 0, fenced: 0, renders: 0 }
    const book = (cmd, args) => coord.book(cmd, args)

    const active = () => plugins.size > 0
    const srcKey = s => s ? `${s.exchange}/${s.market}/${s.symbol}` : ""

    function withdraw() {
        book("withdraw")
        lastFrame = null
        for (const p of plugins.values()) p.shown = null
    }

    function setStatus(state, reason = null) {
        status = { state, reason }
        if (state !== "live") withdraw()
        render()
    }

    function clearTimers() {
        if (reconnectTimer) { timers.clearTimeout(reconnectTimer); reconnectTimer = null }
        if (staleTimer) { timers.clearTimeout(staleTimer); staleTimer = null }
    }

    function disconnect() {
        epoch++
        clearTimers()
        if (ws) { const s = ws; ws = null; s.onopen = s.onmessage = s.onclose = s.onerror = null; try { s.close() } catch (_) {} }
    }

    function armStale() {
        if (staleTimer) timers.clearTimeout(staleTimer)
        const e = epoch
        staleTimer = timers.setTimeout(() => { if (e === epoch && status.state === "live") setStatus("stale", "no_update") }, BOOK_STALE_MS)
    }

    // Result age is re-evaluated by the coordinator on a clock, not by frames.
    function armView() {
        if (viewTimer || !active()) return
        viewTimer = timers.setTimeout(() => { viewTimer = null; render(); armView() }, VIEW_TICK_MS)
    }

    function connect() {
        disconnect()
        if (!active() || !source) { setStatus("off"); return }
        if (!bookSupported(source)) { setStatus("unsupported"); return }
        const e = epoch
        const want = { ...source }
        setStatus("connecting")
        let s
        try { s = new WebSocketImpl(wsUrl(want)) } catch (_) { scheduleReconnect(e); return }
        ws = s
        s.onmessage = ev => {
            if (e !== epoch) { counters.fenced++; return }
            let f
            try { f = JSON.parse(ev.data) } catch (_) { return }
            if (f?.type !== "book" || srcKey(f.source) !== srcKey(want)) { counters.fenced++; return }
            counters.frames++
            attempt = 0
            onFrame(f)
        }
        s.onclose = () => { if (e === epoch) { ws = null; setStatus("disconnected"); scheduleReconnect(e) } }
        s.onerror = () => {}
    }

    function scheduleReconnect(e) {
        if (e !== epoch || reconnectTimer) return
        const delay = Math.min(10000, 500 * 2 ** Math.min(attempt++, 5))
        reconnectTimer = timers.setTimeout(() => { reconnectTimer = null; if (e === epoch) connect() }, delay)
    }

    function onFrame(f) {
        if (f.status !== "live" || !Array.isArray(f.bids) || !f.bids.length || !f.asks?.length) {
            setStatus(f.status === "live" ? "syncing" : String(f.status), f.reason ?? null)
            return
        }
        const r = book("frame", { source: srcKey(f.source), generation: f.generation, seq: f.seq, received_ms: now() })
        if (!r.admit) { counters.fenced++; return }
        f.received_ms = now()
        lastFrame = f
        if (status.state !== "live") status = { state: "live", reason: null }
        armStale()
        run(r.actions)
        render()
    }

    function bodyOf(f) {
        return JSON.stringify({ source: f.source, base: f.base, quote: f.quote, generation: f.generation, seq: f.seq,
            exchange_time_ms: f.exchange_time_ms, bids: f.bids, asks: f.asks, coverage: f.coverage })
    }

    // Execute coordinator-planned computes; each is bound to the frame it was
    // planned from (always the latest admitted frame at planning time).
    function run(actions) {
        for (const a of actions ?? []) {
            const p = plugins.get(a.plugin)
            const f = lastFrame
            if (!p || !f || f.seq !== a.seq || f.generation !== a.generation) { complete(a.plugin, a.token, "superseded"); continue }
            p.inflight.set(a.token, f)
            counters.computes++
            host.compute(p.instance, `book:${srcKey(f.source)}`, f.generation, BOOK_OP, bodyOf(f)).then(
                text => complete(a.plugin, a.token, null, text, p),
                err => complete(a.plugin, a.token, err?.code || "error", null, p, publicCode(err)))
        }
    }

    // Candidate public classification (`code: fixed message` from the host);
    // the coordinator keeps it only if SDK-allowlisted.
    function publicCode(err) {
        const m = /^([a-z_]{1,32}):/.exec(String(err?.message ?? ""))
        return m ? m[1] : null
    }

    function complete(id, token, error, text = null, p = plugins.get(id), public_code = null) {
        const f = p?.inflight.get(token)
        p?.inflight.delete(token)
        let out = null
        if (!error) { try { out = JSON.parse(text) } catch (_) { error = "invalid_output" } }
        const r = book("done", { plugin: id, token, error, public_code })
        if (!r.admit || plugins.get(id) !== p) counters.fenced++
        else if (!error) p.shown = { token, frame: f, out }
        else p.shown = null
        run(r.actions)
        render()
    }

    async function start(p) {
        let ok = true, code = null, pub = null
        try { await host.load(p.instance, p.spec.bytes, JSON.stringify(p.spec.params ?? {})) } catch (err) { ok = false; code = err?.code || "load_failed"; pub = publicCode(err) }
        if (plugins.get(p.id) !== p) { host.disable(p.instance); return }
        const r = book("loaded", { plugin: p.id, load_key: p.loadKey, ok, error: code, public_code: pub })
        if (r.admit) run(r.actions)
        render()
    }

    function setPlugins(specs) {
        const r = book("plugins", { list: specs.map(s => ({ id: s.id, load_key: s.loadKey })) })
        const want = new Map(specs.map(s => [s.id, s]))
        for (const [id, p] of plugins) {
            if (!want.has(id) || want.get(id).loadKey !== p.loadKey) { host.disable(p.instance); plugins.delete(id) }
        }
        for (const id of r.start ?? []) {
            const s = want.get(id)
            const p = { id, name: s.name || id, loadKey: s.loadKey, spec: s, instance: nextInstance++, inflight: new Map(), shown: null }
            plugins.set(id, p)
            start(p)
        }
        if (!active()) { disconnect(); setStatus("off") }
        else {
            armView()
            if (!ws && !reconnectTimer && status.state !== "unsupported") connect()
            else render()
        }
    }

    function setSource(src) {
        const next = src ? { exchange: src.exchange, market: src.market, symbol: src.symbol } : null
        if (srcKey(next) === srcKey(source)) return
        source = next
        book("source", { source: srcKey(next) })
        lastFrame = null
        // Cancel old-source work; a Worker the host terminated (public
        // supersede result > 0) is reloaded under the same identity.
        for (const p of plugins.values()) {
            p.shown = null
            if (host.supersede(p.instance, `book:${srcKey(next)}`, 0) > 0) start(p)
        }
        if (active()) connect(); else render()
    }

    function venueLabel(f) {
        const src = f?.source || source
        if (!src) return ""
        const m = src.market === "usdm" ? "USDⓈ-M perpetual" : src.market
        return `${src.exchange === "binance" ? "Binance" : src.exchange} ${m} ${src.symbol}${f?.quote ?? (src.market === "usdm" ? "USDT" : "")}`
    }

    function statusText() {
        switch (status.state) {
            case "live": return "live"
            case "unsupported": return "unavailable for this source (Binance USDⓈ-M BTC/ETH only)"
            case "connecting": return "connecting…"
            case "syncing": return "syncing…"
            case "stale": return `stale${status.reason ? ` (${status.reason})` : ""} — levels withdrawn`
            case "disconnected": return "disconnected — reconnecting"
            default: return status.state
        }
    }

    // Admitted, fresh view for a plugin (coordinator decides; token must match).
    function current(p) {
        const v = views.get(p.id)
        if (!v || status.state !== "live" || !p.shown || v.token !== p.shown.token) return null
        return ["live", "updating"].includes(v.state) ? p.shown : null
    }

    const scrolls = new Map()
    const restored = new WeakMap()
    let faultSig = ""
    function render() {
        counters.renders++
        views = new Map((book("view", { now_ms: now() }).plugins ?? []).map(v => [v.plugin, v]))
        // Notify (e.g. indicator menu) only when fault status actually changes.
        const sig = JSON.stringify([...views.values()].map(v => [v.plugin, v.fault, v.hint]))
        if (sig !== faultSig) { faultSig = sig; if (onChange) onChange() }
        if (!container || typeof container.replaceChildren !== "function") return
        container.hidden = !active()
        if (!active()) { container.replaceChildren(); return }
        const doc = container.ownerDocument
        const el = (tag, cls, text) => { const e = doc.createElement(tag); if (cls) e.className = cls; if (text != null) e.textContent = text; return e }
        const head = el("div", "book-head")
        head.append(el("span", "book-venue", venueLabel(lastFrame)), el("span", `book-status book-${status.state}`, statusText()))
        head.dataset.state = status.state
        const blocks = [head]
        for (const p of plugins.values()) {
            const box = el("div", "book-plugin")
            box.dataset.plugin = p.id
            const v = views.get(p.id)
            const shown = current(p)
            const r = shown?.out
            const f = shown?.frame
            const d = r ? priceDigits(r.rows) : 0
            const age = v?.age_ms != null ? `${(v.age_ms / 1000).toFixed(1)}s` : ""
            const meta = v?.fault ? `fault ${v.hint ?? v.fault}` : v?.state === "starting" ? "starting…" : v?.state === "stale" ? "result stale — withdrawn" : r ? `qty ${r.unit} · ${v.state === "updating" ? "updating" : "live"} ${age}` : ""
            box.dataset.view = v?.state ?? "none"
            box.append(el("div", "book-title", `${p.name}${meta ? " · " + meta : ""}`))
            if (r) {
                const max = Math.max(...r.rows.map(x => Math.max(x.bid, x.ask)), 1e-300)
                const row = x => {
                    const e = el("div", "book-row")
                    e.dataset.side = x.ask > 0 && x.bid > 0 ? "both" : x.ask > 0 ? "ask" : "bid"
                    const bid = el("span", "book-bid", qtyFmt(x.bid)); bid.style.setProperty("--w", `${(100 * x.bid / max).toFixed(1)}%`)
                    const ask = el("span", "book-ask", qtyFmt(x.ask)); ask.style.setProperty("--w", `${(100 * x.ask / max).toFixed(1)}%`)
                    e.append(bid, el("span", "book-price", fmt(x.price, d)), ask)
                    return e
                }
                // Asks above (nearest at the bottom, scrolled to the spread),
                // bids below (nearest at the top): both sides visible at once.
                const asks = el("div", "book-rows book-asks"); asks.dataset.scroll = `${p.id}:asks`
                const bids = el("div", "book-rows book-bids"); bids.dataset.scroll = `${p.id}:bids`
                asks.setAttribute("role", "table"); bids.setAttribute("role", "table")
                asks.setAttribute("aria-label", `asks, quantities in ${r.unit}`); bids.setAttribute("aria-label", `bids, quantities in ${r.unit}`)
                const askRows = r.rows.filter(x => x.price >= r.best_ask || (x.ask > 0 && x.bid === 0)).sort((a, b) => b.price - a.price)
                for (const x of askRows) asks.append(row(x))
                for (const x of r.rows.filter(x => !askRows.includes(x))) bids.append(row(x))
                const spread = el("div", "book-spread", `bid ${fmt(r.best_bid, d)} · spread ${fmt(r.best_ask - r.best_bid, d)} · ask ${fmt(r.best_ask, d)}`)
                box.append(asks, spread, bids)
                const cov = f?.coverage
                if (cov) box.append(el("div", "book-note", `Plugin view of supplied depth (resting, single source; seq ${f.seq}). Supplied range ${fmt(cov.bid_floor, d)}–${fmt(cov.ask_ceiling, d)}; outside is unknown.`))
            }
            blocks.push(box)
        }
        container.replaceChildren(...blocks)
        // Manual scroll is recorded from scroll events (not re-derived from
        // geometry at render/resize time); a section left at its spread edge
        // stays pinned there.
        for (const e of container.querySelectorAll("[data-scroll]")) {
            e.addEventListener("scroll", () => {
                // Scroll events are delivered a frame later; one that only
                // reports pin()'s own restoration (possibly read against
                // newer geometry) or a detached section is not user intent.
                // The marker is consumed so a later return to it counts.
                const r = restored.get(e); restored.delete(e)
                if (!e.isConnected || e.scrollTop === r) return
                const atEdge = e.dataset.scroll.endsWith(":asks") ? e.scrollHeight - e.clientHeight - e.scrollTop <= 2 : e.scrollTop <= 2
                scrolls.set(e.dataset.scroll, atEdge ? null : e.scrollTop)
            }, { passive: true })
        }
        pin()
    }

    // Default places the spread in view on both sides; keeps manual scroll.
    function pin() {
        for (const e of container?.querySelectorAll?.("[data-scroll]") ?? []) {
            const saved = scrolls.get(e.dataset.scroll)
            e.scrollTop = saved != null ? saved : e.dataset.scroll.endsWith(":asks") ? e.scrollHeight : 0
            restored.set(e, e.scrollTop)
        }
    }
    // Layout changes (viewport/panel resize) keep pinned sections at the spread.
    if (container && typeof globalThis.ResizeObserver === "function") new globalThis.ResizeObserver(() => pin()).observe(container)

    return {
        setPlugins,
        setSource,
        destroy() { disconnect(); if (viewTimer) { timers.clearTimeout(viewTimer); viewTimer = null } for (const p of plugins.values()) host.disable(p.instance); plugins.clear(); book("plugins", { list: [] }); render() },
        state: () => ({ status: { ...status }, source: source && { ...source }, epoch, connected: !!ws, generation: lastFrame?.generation ?? null, seq: lastFrame?.seq ?? null,
            plugins: [...plugins.values()].map(p => {
                const v = views.get(p.id), s = current(p)
                return { id: p.id, state: v?.fault ? (v.fatal ? "fault" : "ready") : v?.state === "starting" ? "starting" : "ready",
                    fault: v?.fault ?? null, publicCode: v?.public_code ?? null, hint: v?.hint ?? null, rows: s?.out?.rows?.length ?? 0, unit: s?.out?.unit ?? null,
                    view: v?.state ?? null, seq: s?.frame.seq ?? null, generation: s?.frame.generation ?? null, bestBid: s?.out?.best_bid ?? null, ageMs: v?.age_ms ?? null }
            }), counters: { ...counters } }),
    }
}

// Compact indicator multiselect: one trigger button in the toolbar and one
// popup of native checkbox rows. The popup lives OUTSIDE the toolbar (which
// is overflow:auto for short viewports and would clip an absolute child) and
// is placed with position:fixed from the trigger's rect, clamped to the
// viewport. Rows are keyed by id and updated in place so ordinary rerenders
// keep the popup open and focus where it is. Knows nothing about specific
// plugin ids; callers pass generic row items.
//
// items: [{ id, name, meta, title, checked, disabled }]
export function createIndicatorMenu({ container, popup, onToggle, label = "Indicators" }) {
    const doc = globalThis.document
    const win = globalThis.window ?? {}
    const make = (tag, className) => { const el = doc.createElement(tag); if (className) el.className = className; return el }

    const trigger = make("button", "toolbar-btn plugin-menu-trigger")
    trigger.type = "button"
    // Disclosure button (aria-expanded/controls) for a labelled checkbox
    // group; deliberately no aria-haspopup/menu roles, whose arrow-key menu
    // interaction model this popup does not implement.
    trigger.setAttribute("aria-expanded", "false")
    trigger.setAttribute("aria-controls", popup.id || "plugin-menu")
    const triggerText = make("span", "plugin-menu-count")
    const caret = make("span", "plugin-menu-caret")
    caret.setAttribute("aria-hidden", "true")
    caret.textContent = "▾"
    trigger.append(triggerText, caret)
    container.replaceChildren(trigger)

    const heading = make("div", "plugin-menu-heading")
    heading.id = (popup.id || "plugin-menu") + "-heading"
    heading.textContent = label
    const list = make("div", "plugin-menu-list")
    list.setAttribute("role", "group")
    list.setAttribute("aria-labelledby", heading.id)
    const empty = make("p", "plugin-menu-empty")
    empty.dataset.empty = ""
    empty.textContent = "No indicators available"
    popup.hidden = true
    popup.replaceChildren(heading, list)

    const rows = new Map()
    let order = []
    let open = false

    function row(item) {
        let r = rows.get(item.id)
        if (!r) {
            const el = make("label", "plugin-menu-row")
            el.dataset.plugin = item.id
            const input = make("input")
            input.type = "checkbox"
            const name = make("span", "plugin-menu-name")
            const meta = make("span", "plugin-menu-meta")
            el.append(input, name, meta)
            r = { el, input, name, meta }
            input.onchange = () => r.onchange?.(input.checked)
            rows.set(item.id, r)
        }
        // Write only on change: avoids churn and keeps focus stable.
        if (r.el.title !== item.title) r.el.title = item.title
        if (r.name.textContent !== item.name) r.name.textContent = item.name
        if (r.meta.textContent !== (item.meta ?? "")) r.meta.textContent = item.meta ?? ""
        if (r.input.checked !== item.checked) r.input.checked = item.checked
        if (r.input.disabled !== item.disabled) r.input.disabled = item.disabled
        r.el.dataset.state = item.disabled ? "unavailable" : "ready"
        r.onchange = item.disabled ? null : checked => onToggle(item.id, checked)
        return r.el
    }

    function render(items, selected) {
        const ids = items.map(item => item.id)
        const els = items.map(row)
        for (const id of [...rows.keys()]) if (!ids.includes(id)) rows.delete(id)
        const next = els.length ? els : [empty]
        if (next.length !== order.length || next.some((el, i) => el !== order[i])) {
            // Move only out-of-place nodes: re-inserting a focused row would
            // blur it, so unchanged rows stay put across discovery updates.
            if (typeof list.insertBefore === "function") {
                next.forEach((el, i) => { if (list.children[i] !== el) list.insertBefore(el, list.children[i] ?? null) })
                while (list.children.length > next.length) list.lastChild.remove()
            } else list.replaceChildren(...next)
            order = next
        }
        const text = `${label} · ${selected}`
        if (triggerText.textContent !== text) triggerText.textContent = text
        trigger.title = `${selected} of ${items.filter(item => !item.disabled).length} indicators selected`
        if (open) position()
    }

    // Fixed placement from the trigger rect, clamped to the VISIBLE region:
    // visualViewport origin (offsetLeft/Top, non-zero under pinch-zoom/pan)
    // and extent. Falls back to the layout viewport when unsupported.
    function position() {
        const vv = win.visualViewport
        const view = vv ? { x: vv.offsetLeft ?? 0, y: vv.offsetTop ?? 0, width: vv.width, height: vv.height }
            : { x: 0, y: 0, width: win.innerWidth ?? 0, height: win.innerHeight ?? 0 }
        const layoutH = win.innerHeight ?? view.height
        if (!view.width || !view.height || typeof trigger.getBoundingClientRect !== "function") return
        const p = placeIndicatorMenu(trigger.getBoundingClientRect(), view, layoutH)
        Object.assign(popup.style, {
            width: `${p.width}px`, maxHeight: `${p.maxHeight}px`, left: `${p.left}px`,
            top: p.up ? "auto" : `${p.top}px`, bottom: p.up ? `${p.bottomPx}px` : "auto",
        })
    }

    function setOpen(next, { focusTrigger = false } = {}) {
        if (open === next) return
        open = next
        popup.hidden = !open
        trigger.setAttribute("aria-expanded", String(open))
        trigger.classList.toggle?.("active", open)
        if (open) position()
        if (!open && focusTrigger) trigger.focus?.()
    }

    const inside = node => popup.contains?.(node) || trigger.contains?.(node)
    function handlePointer(event) { if (open && !inside(event.target)) setOpen(false) }
    function handleKeydown(event) {
        if (!open) return
        if (event.key === "Escape") { event.preventDefault?.(); setOpen(false, { focusTrigger: true }); return }
        if (event.key !== "Tab") return
        // First/last come from the CURRENT rendered order (not Map order).
        const enabled = order.map(rowInput).filter(input => input && !input.disabled)
        const first = enabled[0], last = enabled.at(-1)
        if (event.target === trigger) {
            // The popup sits after the toolbar in the DOM: Tab from the
            // trigger enters it; Shift+Tab, or nothing enabled, leaves
            // natively and closes.
            if (!event.shiftKey && first) { event.preventDefault?.(); first.focus(); return }
            setOpen(false)
        } else if (event.shiftKey && event.target === first) {
            event.preventDefault?.(); trigger.focus()
        } else if (!event.shiftKey && event.target === last) {
            // Continue with the toolbar control after the trigger (e.g. Agg).
            const next = nextFocusable(trigger)
            setOpen(false)
            if (next) { event.preventDefault?.(); next.focus() }
        }
    }
    const rowInput = el => [...rows.values()].find(r => r.el === el)?.input
    function nextFocusable(from) {
        if (typeof doc.querySelectorAll !== "function") return null
        const all = [...doc.querySelectorAll("button, input, select, textarea, a[href], [tabindex]")].filter(el =>
            el === from || (!el.disabled && el.tabIndex >= 0 && !popup.contains(el) && el.getClientRects().length > 0))
        const i = all.indexOf(from)
        return i >= 0 ? all[i + 1] ?? null : null
    }
    function handleFocusOut(event) { if (open && event.relatedTarget && !inside(event.relatedTarget)) setOpen(false) }

    trigger.onclick = () => setOpen(!open)
    // Document-level so Escape still closes after a discovery update removed
    // the focused row (focus falls back to <body>).
    doc.addEventListener?.("keydown", handleKeydown)
    popup.addEventListener?.("focusout", handleFocusOut)
    doc.addEventListener?.("pointerdown", handlePointer, true)
    const reposition = () => { if (open) position() }
    win.addEventListener?.("resize", reposition)
    win.visualViewport?.addEventListener?.("resize", reposition)
    win.visualViewport?.addEventListener?.("scroll", reposition) // pinch-zoom pan
    doc.addEventListener?.("scroll", reposition, true) // includes toolbar scroll

    return { render, trigger, handleKeydown, handlePointer, isOpen: () => open, close: () => setOpen(false) }
}

// Pure placement math (unit-tested with synthetic viewports). rect: trigger
// client rect; view: visible region {x, y, width, height} in layout-viewport
// coordinates; layoutH: layout viewport height (for CSS `bottom`).
export function placeIndicatorMenu(rect, view, layoutH) {
    const gap = 4, margin = 8
    const vLeft = view.x, vTop = view.y, vRight = view.x + view.width, vBottom = view.y + view.height
    const width = Math.min(340, view.width - margin * 2)
    const below = vBottom - Math.max(rect.bottom, vTop) - gap - margin
    const above = Math.min(rect.top, vBottom) - vTop - gap - margin
    const up = below < 180 && above > below
    const maxHeight = Math.max(80, Math.min(420, up ? above : below, view.height - margin * 2))
    const left = Math.round(Math.min(Math.max(vLeft + margin, rect.left), vRight - width - margin))
    const top = Math.round(Math.max(vTop + margin, Math.min(rect.bottom + gap, vBottom - margin - maxHeight)))
    const anchor = Math.min(vBottom - margin, Math.max(vTop + margin + maxHeight, rect.top - gap))
    return { up, width, maxHeight, left, top, bottomPx: Math.round(layoutH - anchor) }
}

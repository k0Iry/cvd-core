// One-time migration of pre-plugin preferences (cvd_prefs_v1 written by the
// fixed-indicator UI) into the generic plugin preference shape. This is the
// ONLY place that names historical indicator ids; renderers stay generic.
// Runs only when the stored prefs have no `plugins` block yet; the next save
// writes the new shape, so it never re-applies.
const LEGACY_FLOW_MODES = { "buy-sell": "flow/buy_sell", delta: "flow/delta" }

export function migrateLegacyPrefs(prefs) {
    if (!prefs || typeof prefs !== "object" || prefs.plugins) return prefs
    const plugins = { enabled: {}, modes: {} }
    if (typeof prefs.showLevels === "boolean") plugins.enabled.levels = prefs.showLevels
    if (Object.hasOwn(LEGACY_FLOW_MODES, prefs.flowMode)) plugins.modes["flow:flow"] = LEGACY_FLOW_MODES[prefs.flowMode]
    const { showLevels, flowMode, ...rest } = prefs
    return { ...rest, plugins }
}

// EXPLICIT real-browser integration (not in the default node-unit globs):
//   CVD_CHROMIUM=<chromium> node --test tests/chromium-launch.browser.mjs
// Moved verbatim from chromium-launch.test.mjs. A missing browser FAILS: an
// explicitly requested browser test never reports a skipped success.
import test from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { launchChromium } from './chromium-launch.mjs'
import { teardown } from './harness-teardown.mjs'

const mk = () => fs.mkdtempSync(path.join(process.env.TMPDIR || os.tmpdir(), 'cvd-launch-'))
async function run(bin, budgetMs, dir, pollMs) {
  const profile = path.join(dir, 'p'); fs.mkdirSync(profile)
  const t0 = Date.now(), { child, promise } = launchChromium({ bin, profile, budgetMs, ...(pollMs ? { pollMs } : {}) })
  let err, ok; try { ok = await promise } catch (e) { err = e }
  const td = await teardown({ child, profileDir: profile, opts: { graceMs: 1000, killMs: 2000 } })
  return { err, ok, td, ms: Date.now() - t0, budgetMs }
}

const real = process.env.CVD_CHROMIUM || '/usr/bin/chromium'
test('real Chromium reaches CDP readiness', async () => {
  assert.ok(fs.existsSync(real), `browser required for this explicit test: ${real} not found (set CVD_CHROMIUM)`)
  const d = mk(); const r = await run(real, 20000, d)
  assert.ifError(r.err); assert.match(r.ok.wsUrl, /^ws:\/\/127\.0\.0\.1:\d+\/devtools\/browser\//)
  // Both discovery paths are valid and each is CDP-probe verified; which one wins the race is not the contract.
  assert.ok(['DevToolsActivePort', 'stderr-banner'].includes(r.ok.via), r.ok.via)
  assert.match(r.ok.httpBase, /^http:\/\/127\.0\.0\.1:\d+$/); assert.ok(r.ok.ms <= 20000, `${r.ok.ms}ms`)
  assert.equal(r.td.ok, true); fs.rmSync(d, { recursive: true })
})

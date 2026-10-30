// Bounded, classified Chromium startup for real-browser test harnesses.
// Readiness = owned <profile>/DevToolsActivePort (or the stderr banner) AND a
// real CDP HTTP answer from /json/version, fully received within ONE absolute
// startup budget. Fails promptly and honestly on spawn error (missing/
// non-executable binary) or early exit, and on the budget, always with a
// stderr tail capped at STDERR_CAP bytes for diagnosis.
// The caller keeps ownership of `child` and must run its existing teardown.
import fs from 'node:fs'
import path from 'node:path'
import { spawn } from 'node:child_process'

export const STDERR_CAP = 4096 // UTF-8 bytes, not JS characters

export class ChromiumStartError extends Error {
  constructor(kind, detail, stderrTail) {
    super(`chromium did not start [${kind}]: ${detail}` + (stderrTail ? `\n--- chromium stderr (last ${STDERR_CAP} bytes max) ---\n${stderrTail}` : '\n(no chromium stderr captured)'))
    this.kind = kind; this.stderrTail = stderrTail
  }
}

const sleep = ms => new Promise(r => setTimeout(r, ms))
const positive = (name, v) => { if (!Number.isFinite(v) || v <= 0) throw new TypeError(`${name} must be a finite positive number`); return v }

// Decode the last <= cap bytes, dropping a split leading/trailing UTF-8
// sequence so the result is valid text whose UTF-8 size is <= cap.
export function byteTail(buf, cap = STDERR_CAP) {
  const b = buf.length > cap ? buf.subarray(buf.length - cap) : buf
  let i = 0
  while (i < b.length && i < 3 && (b[i] & 0xc0) === 0x80) i++
  const text = new TextDecoder().decode(b.subarray(i)).replace(/\uFFFD$/, '')
  // Invalid bytes decode to U+FFFD (3 UTF-8 bytes); re-bound the decoded text.
  const enc = Buffer.from(text, 'utf8')
  if (enc.length <= cap) return text
  let j = enc.length - cap
  while (j < enc.length && (enc[j] & 0xc0) === 0x80) j++
  return enc.subarray(j).toString('utf8')
}

// Returns { child, promise }: child is available synchronously so the caller
// can always tear it down; promise resolves { wsUrl, httpBase, via, ms }.
export function launchChromium({ bin, profile, extraArgs = [], budgetMs = 20000, pollMs = 100, spawnFn = spawn } = {}) {
  positive('budgetMs', budgetMs); positive('pollMs', pollMs)
  const args = ['--headless=new', '--no-sandbox', '--disable-dev-shm-usage', '--disable-gpu',
    '--no-first-run', '--remote-debugging-port=0', '--user-data-dir=' + profile, ...extraArgs, 'about:blank']
  const t0 = Date.now(), deadline = t0 + budgetMs
  let raw = Buffer.alloc(0)
  let failure = null
  let banner = null
  const tail = () => byteTail(raw)
  const child = spawnFn(bin, args, { stdio: ['ignore', 'ignore', 'pipe'] })
  child.stderr?.on('data', d => {
    raw = Buffer.concat([raw, d]); if (raw.length > STDERR_CAP) raw = raw.subarray(raw.length - STDERR_CAP)
    const m = raw.toString('latin1').match(/DevTools listening on (ws:\/\/\S+)/); if (m) banner = m[1]
  })
  child.on('error', e => { failure ??= ['spawn-error', `${e.code || ''} ${e.message}`.trim()] })
  child.on('exit', (code, sig) => { failure ??= ['exited-early', `exit code=${code} signal=${sig}`] })
  const portFile = path.join(profile, 'DevToolsActivePort')
  const failed = async () => { await sleep(50); return new ChromiumStartError(failure[0], `${failure[1]} after ${Date.now() - t0}ms (bin=${bin})`, tail()) }
  const timedOut = why => new ChromiumStartError('timeout', `no CDP readiness within ${budgetMs}ms (${why}; bin=${bin})`, tail())
  const promise = (async () => {
    let endpoint = null, lastProbe = 'no endpoint yet'
    while (true) {
      if (failure) throw await failed()
      if (Date.now() >= deadline) throw timedOut(lastProbe)
      if (!endpoint) {
        try {
          const [port, p] = fs.readFileSync(portFile, 'utf8').split('\n')
          if (/^\d+$/.test(port) && p) endpoint = { host: `127.0.0.1:${port}`, via: 'DevToolsActivePort' }
        } catch (_) {}
        if (!endpoint && banner) endpoint = { host: new URL(banner).host, via: 'stderr-banner' }
      }
      if (endpoint) {
        let v = null
        try {
          // One signal bounds the whole probe (headers AND body) by the
          // remaining absolute budget; no floor that could outlive it.
          const signal = AbortSignal.timeout(Math.max(1, Math.min(2000, deadline - Date.now())))
          const r = await fetch(`http://${endpoint.host}/json/version`, { signal })
          v = await r.json()
        } catch (e) { lastProbe = `CDP probe: ${e.name}` }
        // Re-check after the awaits: a known exit or a passed deadline wins
        // over a late answer.
        if (failure) throw await failed()
        if (Date.now() > deadline) throw timedOut(v ? 'CDP answered only after the deadline' : lastProbe)
        if (v?.webSocketDebuggerUrl) return { wsUrl: v.webSocketDebuggerUrl, httpBase: `http://${endpoint.host}`, via: endpoint.via, ms: Date.now() - t0, browser: v.Browser }
        if (v) lastProbe = 'CDP /json/version without webSocketDebuggerUrl'
      }
      await sleep(Math.min(pollMs, Math.max(1, deadline - Date.now())))
    }
  })()
  promise.catch(() => {})
  return { child, promise }
}

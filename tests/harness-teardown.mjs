// Bounded teardown for browser test harnesses that own one child browser and
// one temporary profile directory. Never signals anything except the owned
// child and never removes anything except the exact owned path.
import fs from 'node:fs'

const sleep = ms => new Promise(r => setTimeout(r, ms))
const exited = child => child.exitCode !== null || child.signalCode !== null

// Wait for the owned child to exit (be reaped): SIGTERM, then SIGKILL after
// graceMs, then give up after killMs. Returns { ok, how }.
export async function terminateChild(child, { graceMs = 3000, killMs = 5000 } = {}) {
  if (!child || exited(child)) return { ok: true, how: 'already-exited' }
  const done = new Promise(r => child.once('exit', () => r(true)))
  const wait = ms => Promise.race([done, sleep(ms).then(() => false)])
  try { child.kill('SIGTERM') } catch (_) {}
  if (await wait(graceMs) || exited(child)) return { ok: true, how: 'SIGTERM' }
  try { child.kill('SIGKILL') } catch (_) {}
  if (await wait(killMs) || exited(child)) return { ok: true, how: 'SIGKILL' }
  return { ok: false, how: `pid ${child.pid} still running after ${graceMs + killMs}ms` }
}

const TRANSIENT = new Set(['ENOTEMPTY', 'EBUSY', 'EPERM'])

// Remove the exact owned directory with a bounded retry on transient errors.
export async function removeOwnedDir(dir, { attempts = 10, delayMs = 200, rm = fs.rmSync, exists = fs.existsSync } = {}) {
  let last
  for (let i = 1; i <= attempts; i++) {
    try {
      rm(dir, { recursive: true, force: true })
      if (!exists(dir)) return { ok: true, attempts: i }
      last = new Error(`${dir} still present`)
    } catch (error) {
      last = error
      if (!TRANSIENT.has(error.code)) return { ok: false, attempts: i, error: `${error.code || ''} ${error.message}` }
    }
    await sleep(delayMs)
  }
  return { ok: false, attempts, error: `${last?.code || ''} ${last?.message}` }
}

export function closeServer(server, sockets, { timeoutMs = 3000 } = {}) {
  for (const s of sockets) s.destroy()
  server.closeAllConnections?.()
  return Promise.race([
    new Promise(r => server.close(err => r({ ok: !err || err.code === 'ERR_SERVER_NOT_RUNNING', error: err?.message }))),
    sleep(timeoutMs).then(() => ({ ok: false, error: 'server.close timeout' })),
  ]).then(r => ({ ...r, ok: r.ok && !server.listening && [...sockets].every(s => s.destroyed) }))
}

// Helper processes (renderer/network/GPU) of the owned browser carry the
// exact --user-data-dir argument; they can outlive the main pid briefly and
// recreate files after removal. Scan /proc for that exact argument only.
export function ownedHelpers(profileDir) {
  const want = '--user-data-dir=' + profileDir, out = []
  let ids = []; try { ids = fs.readdirSync('/proc').filter(d => /^\d+$/.test(d)) } catch (_) { return out }
  for (const id of ids) {
    if (Number(id) === process.pid) continue
    try { if (fs.readFileSync(`/proc/${id}/cmdline`, 'utf8').split('\0').includes(want)) out.push(Number(id)) } catch (_) {}
  }
  return out
}

export async function reapHelpers(profileDir, { killMs = 5000, scan = ownedHelpers } = {}) {
  const deadline = Date.now() + killMs
  for (;;) {
    const left = scan(profileDir)
    if (!left.length) return { ok: true }
    if (Date.now() > deadline) return { ok: false, how: `helpers alive: ${left.join(',')}` }
    for (const pid of left) { try { process.kill(pid, 'SIGKILL') } catch (_) {} }
    await sleep(50)
  }
}

// Full teardown: child exit -> server/sockets closed -> profile removed.
export async function teardown({ child, server, sockets, profileDir, opts = {} }) {
  const main = await terminateChild(child, opts)
  const helpers = main.ok ? await reapHelpers(profileDir, opts) : { ok: false }
  const browser = { ...main, ok: main.ok && helpers.ok, helpers: helpers.ok ? 'none' : helpers.how }
  const net = server ? await closeServer(server, sockets ?? new Set(), opts) : { ok: true }
  // Only remove the profile once its owner is gone; a live browser could recreate files.
  let profile = browser.ok ? await removeOwnedDir(profileDir, opts) : { ok: false, error: 'browser not reaped; profile kept' }
  // Completion means the profile stays gone: re-check after a short settle.
  if (profile.ok) { await sleep(opts.settleMs ?? 300); if (fs.existsSync(profileDir)) profile = { ok: false, error: 'profile reappeared after removal' } }
  return { ok: browser.ok && net.ok && profile.ok, browser, server: net, profile }
}

import test from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import http from 'node:http'
import { spawn } from 'node:child_process'
import { terminateChild, removeOwnedDir, teardown } from './harness-teardown.mjs'

const tmp = () => fs.mkdtempSync(path.join(process.env.TMPDIR || os.tmpdir(), 'harness-teardown-'))
const alive = pid => { try { process.kill(pid, 0); return true } catch (_) { return false } }

test('transient ENOTEMPTY is retried until the owned dir is gone', async () => {
  const dir = tmp(); fs.writeFileSync(path.join(dir, 'f'), 'x')
  let calls = 0
  const rm = (d, o) => { if (++calls < 3) { const e = new Error('ENOTEMPTY: directory not empty'); e.code = 'ENOTEMPTY'; throw e } fs.rmSync(d, o) }
  const r = await removeOwnedDir(dir, { rm, delayMs: 5 })
  assert.deepEqual(r, { ok: true, attempts: 3 }); assert.equal(fs.existsSync(dir), false)
})

test('persistent removal failure is reported (control), not swallowed', async () => {
  const dir = tmp()
  const rm = () => { const e = new Error('ENOTEMPTY'); e.code = 'ENOTEMPTY'; throw e }
  const r = await removeOwnedDir(dir, { rm, attempts: 3, delayMs: 5 })
  assert.equal(r.ok, false); assert.equal(r.attempts, 3); assert.match(r.error, /ENOTEMPTY/)
  fs.rmSync(dir, { recursive: true })
})

test('non-transient error fails immediately', async () => {
  const rm = () => { const e = new Error('EIO'); e.code = 'EIO'; throw e }
  const r = await removeOwnedDir('/nonexistent-owned', { rm, delayMs: 5 })
  assert.equal(r.ok, false); assert.equal(r.attempts, 1)
})

test('child that ignores SIGTERM is escalated and reaped before profile removal', async () => {
  const dir = tmp()
  // Child keeps writing into the profile until killed (models a browser still flushing).
  const child = spawn(process.execPath, ['-e', `process.on('SIGTERM',()=>{});let i=0;setInterval(()=>require('fs').writeFileSync(${JSON.stringify(dir)}+'/f'+(i++%5),'x'),5)`], { stdio: 'ignore' })
  await new Promise(r => setTimeout(r, 200))
  const server = http.createServer(); await new Promise(r => server.listen(0, '127.0.0.1', r))
  const r = await teardown({ child, server, sockets: new Set(), profileDir: dir, opts: { graceMs: 200, killMs: 2000, delayMs: 20 } })
  assert.equal(r.ok, true, JSON.stringify(r)); assert.equal(r.browser.how, 'SIGKILL')
  assert.equal(alive(child.pid), false); assert.equal(server.listening, false); assert.equal(fs.existsSync(dir), false)
})

test('unreaped child is a real failure and profile is kept (control)', async () => {
  const dir = tmp()
  const fake = { pid: 0, exitCode: null, signalCode: null, kill() {}, once() {} }
  const r = await teardown({ child: fake, profileDir: dir, opts: { graceMs: 20, killMs: 20 } })
  assert.equal(r.ok, false); assert.equal(r.browser.ok, false); assert.equal(fs.existsSync(dir), true)
  fs.rmSync(dir, { recursive: true })
})

test('already-exited child', async () => {
  const child = spawn(process.execPath, ['-e', '0'], { stdio: 'ignore' })
  await new Promise(r => child.once('exit', r))
  assert.deepEqual(await terminateChild(child), { ok: true, how: 'already-exited' })
})

test('server with a live owned socket is closed and socket destroyed', async () => {
  const { closeServer } = await import('./harness-teardown.mjs')
  const net = await import('node:net')
  const sockets = new Set()
  const server = http.createServer(); server.on('connection', s => { sockets.add(s); s.on('close', () => sockets.delete(s)) })
  await new Promise(r => server.listen(0, '127.0.0.1', r))
  const c = net.connect(server.address().port, '127.0.0.1'); c.on('error', () => {})
  await new Promise(r => server.once('connection', () => setTimeout(r, 20)))
  const held = [...sockets]
  const r = await closeServer(server, sockets)
  assert.equal(r.ok, true, JSON.stringify(r)); assert.equal(server.listening, false); assert.ok(held.every(s => s.destroyed))
  c.destroy()
})

test('owned helper outliving main pid is reaped; profile cannot reappear', async () => {
  const dir = tmp()
  const main = spawn(process.execPath, ['-e', 'setInterval(()=>{},1000)'], { stdio: 'ignore' })
  // Helper detached from main, tagged with the exact owned profile argument, keeps writing.
  const helper = spawn(process.execPath, ['-e', `const fs=require('fs');setInterval(()=>{fs.mkdirSync(${JSON.stringify(dir)}+'/Default',{recursive:true});fs.writeFileSync(${JSON.stringify(dir)}+'/Default/x','1')},5)`, '--', '--user-data-dir=' + dir], { stdio: 'ignore', detached: true })
  const other = spawn(process.execPath, ['-e', 'setInterval(()=>{},1000)', '--', '--user-data-dir=' + dir + '-other'], { stdio: 'ignore' })
  await new Promise(r => setTimeout(r, 200))
  assert.ok(alive(helper.pid) && alive(other.pid) && fs.existsSync(dir + '/Default/x'), 'fixture running')
  const r = await teardown({ child: main, profileDir: dir, opts: { graceMs: 500, delayMs: 20, settleMs: 200 } })
  assert.equal(r.ok, true, JSON.stringify(r)); assert.equal(alive(helper.pid), false); assert.equal(fs.existsSync(dir), false)
  assert.equal(alive(other.pid), true, 'non-owned process untouched'); other.kill('SIGKILL')
})

test('control: profile recreated without helper reaping is detected', async () => {
  const dir = tmp()
  const helper = spawn(process.execPath, ['-e', `const fs=require('fs');setInterval(()=>{try{fs.mkdirSync(${JSON.stringify(dir)},{recursive:true})}catch(e){}},5)`], { stdio: 'ignore' })
  const r = await teardown({ child: null, profileDir: dir, opts: { delayMs: 20, settleMs: 200, scan: () => [] } })
  helper.kill('SIGKILL'); await new Promise(r => helper.once('exit', r))
  assert.equal(r.ok, false); assert.match(r.profile.error, /reappeared/)
  fs.rmSync(dir, { recursive: true, force: true })
})

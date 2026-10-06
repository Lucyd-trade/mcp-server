#!/usr/bin/env node
// Launcher: downloads the native lucyd-mcp binary for this platform from GitHub Releases
// on first run, checks it against checksums.json shipped in this package, then runs it.
// stdout belongs to the MCP protocol, so all messages go to stderr.

const { spawn } = require('node:child_process')
const crypto = require('node:crypto')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')

const { version } = require('../package.json')
const REPO = 'lucyd-trade/mcp-server'

const TARGETS = {
  'win32-x64': 'x86_64-pc-windows-msvc',
  'linux-x64': 'x86_64-unknown-linux-gnu',
  'linux-arm64': 'aarch64-unknown-linux-gnu',
  'darwin-x64': 'x86_64-apple-darwin',
  'darwin-arm64': 'aarch64-apple-darwin',
}

function fail(msg) {
  process.stderr.write(`lucyd-mcp: ${msg}\n`)
  process.exit(1)
}

function cacheDir() {
  if (process.platform === 'win32') {
    return path.join(process.env.LOCALAPPDATA || path.join(os.homedir(), 'AppData', 'Local'), 'lucyd-mcp')
  }
  return path.join(process.env.XDG_CACHE_HOME || path.join(os.homedir(), '.cache'), 'lucyd-mcp')
}

async function ensureBinary() {
  const target = TARGETS[`${process.platform}-${process.arch}`]
  if (!target) fail(`no prebuilt binary for ${process.platform}-${process.arch}; build from source, see https://github.com/${REPO}`)

  const exe = process.platform === 'win32' ? '.exe' : ''
  const asset = `lucyd-mcp-${target}${exe}`
  const dest = path.join(cacheDir(), version, `lucyd-mcp${exe}`)
  if (fs.existsSync(dest)) return dest

  let checksums
  try {
    checksums = require('../checksums.json')
  } catch {
    fail('checksums.json is missing from the package')
  }
  const expected = checksums[asset]
  if (!expected) fail(`no checksum for ${asset}`)

  const url = `https://github.com/${REPO}/releases/download/v${version}/${asset}`
  process.stderr.write(`lucyd-mcp: downloading ${url}\n`)
  const res = await fetch(url)
  if (!res.ok) fail(`download failed: HTTP ${res.status}`)
  const data = Buffer.from(await res.arrayBuffer())

  const actual = crypto.createHash('sha256').update(data).digest('hex')
  if (actual !== expected) fail(`checksum mismatch for ${asset}`)

  fs.mkdirSync(path.dirname(dest), { recursive: true })
  const tmp = `${dest}.${process.pid}.tmp`
  fs.writeFileSync(tmp, data, { mode: 0o755 })
  fs.renameSync(tmp, dest)
  return dest
}

ensureBinary()
  .then((bin) => {
    const child = spawn(bin, process.argv.slice(2), { stdio: 'inherit' })
    for (const sig of ['SIGINT', 'SIGTERM']) process.on(sig, () => child.kill(sig))
    child.on('error', (e) => fail(e.message))
    child.on('exit', (code, signal) => process.exit(signal ? 1 : code ?? 0))
  })
  .catch((e) => fail(e.message))

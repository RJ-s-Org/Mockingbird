'use strict';

// The fallback for installs without the platform package (`--omit=optional`,
// `--no-optional`, a lockfile written on another platform): download the
// GitHub release asset once, check it against the sha256 this package was
// published with, and cache the binary.

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { execFileSync } = require('node:child_process');
const { TARGETS, archiveName, binaryName } = require('./targets');

const DEFAULT_BASE_URL = 'https://github.com/RJ-s-Org/Mockingbird/releases/download';

/** Where downloaded binaries are kept (`MOCKINGBIRD_CACHE_DIR` overrides it). */
function cacheDir(env = process.env, platform = process.platform, home = os.homedir()) {
  if (env.MOCKINGBIRD_CACHE_DIR) return env.MOCKINGBIRD_CACHE_DIR;
  if (platform === 'win32') {
    return path.join(env.LOCALAPPDATA || path.join(home, 'AppData', 'Local'), 'mockingbird', 'cache');
  }
  if (platform === 'darwin') return path.join(home, 'Library', 'Caches', 'mockingbird');
  return path.join(env.XDG_CACHE_HOME || path.join(home, '.cache'), 'mockingbird');
}

/** The URL of a release asset (`MOCKINGBIRD_DOWNLOAD_URL` replaces the base, for mirrors). */
function assetUrl(version, key, env = process.env) {
  const base = (env.MOCKINGBIRD_DOWNLOAD_URL || DEFAULT_BASE_URL).replace(/\/+$/, '');
  return `${base}/v${version}/${archiveName(version, key)}`;
}

function sha256(buf) {
  return crypto.createHash('sha256').update(buf).digest('hex');
}

// Windows 10 and later ship bsdtar as System32\tar.exe, which also reads
// zip files; a `tar` found first on PATH (Git for Windows) may not.
function tarCommand(platform) {
  if (platform !== 'win32') return 'tar';
  return path.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'tar.exe');
}

/**
 * Returns the path of the cached binary for `version` on target `key`,
 * downloading it first when it is not cached. Throws on any failure.
 */
async function downloadBinary({
  version,
  key,
  checksums,
  env = process.env,
  platform = process.platform,
  fetchImpl = globalThis.fetch,
  log = (msg) => process.stderr.write(`${msg}\n`),
}) {
  const { target } = TARGETS[key];
  const exe = binaryName(key);
  const dir = path.join(cacheDir(env, platform), 'npm', version, target);
  const dest = path.join(dir, exe);
  if (fs.existsSync(dest)) return dest;

  const expected = checksums && checksums[target];
  if (!expected) {
    throw new Error(`this package has no checksum for ${target}, so it cannot verify a download`);
  }
  if (typeof fetchImpl !== 'function') {
    throw new Error('downloading needs Node.js 18 or later (global fetch)');
  }

  const url = assetUrl(version, key, env);
  log(`mockingbird: downloading ${url} (once; cached in ${dir})`);
  let res;
  try {
    res = await fetchImpl(url);
  } catch (err) {
    throw new Error(`cannot download ${url}: ${err.cause ? err.cause.message : err.message}`);
  }
  if (!res.ok) throw new Error(`cannot download ${url}: HTTP ${res.status}`);
  const archive = Buffer.from(await res.arrayBuffer());
  const actual = sha256(archive);
  if (actual !== expected) {
    throw new Error(`checksum mismatch for ${url}: expected sha256 ${expected}, got ${actual}`);
  }

  // Unpack next to the destination and rename, so a concurrent launcher
  // never sees a half-written binary.
  fs.mkdirSync(dir, { recursive: true });
  const tmp = fs.mkdtempSync(path.join(dir, '.download-'));
  try {
    const file = path.join(tmp, archiveName(version, key));
    fs.writeFileSync(file, archive);
    execFileSync(tarCommand(platform), ['-xf', file, '-C', tmp], { stdio: 'pipe' });
    const unpacked = path.join(tmp, `mockingbird-${version}-${target}`, exe);
    if (!fs.existsSync(unpacked)) throw new Error(`${url} has no mockingbird-${version}-${target}/${exe}`);
    fs.chmodSync(unpacked, 0o755);
    try {
      fs.renameSync(unpacked, dest);
    } catch (err) {
      // Another launcher got there first (Windows will not rename over it).
      if (!fs.existsSync(dest)) throw err;
    }
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
  return dest;
}

module.exports = { DEFAULT_BASE_URL, assetUrl, cacheDir, downloadBinary, sha256 };

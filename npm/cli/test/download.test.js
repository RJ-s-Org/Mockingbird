'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const { execFileSync } = require('node:child_process');
const { after, before, test } = require('node:test');

const { TARGETS, archiveName, binaryName } = require('../lib/targets');
const { assetUrl, cacheDir, downloadBinary, sha256 } = require('../lib/download');

const VERSION = '9.8.7';
// The archive format of this host: a zip on Windows, a tarball elsewhere.
const KEY = process.platform === 'win32' ? 'win32-x64' : 'linux-x64';
const { target: TARGET } = TARGETS[KEY];
const TAR = process.platform === 'win32' ? path.join(process.env.SystemRoot, 'System32', 'tar.exe') : 'tar';

let work;
let server;
let base;
let archive;
let requests = 0;

before(async () => {
  work = fs.mkdtempSync(path.join(os.tmpdir(), 'mb-npm-download-'));
  const top = `mockingbird-${VERSION}-${TARGET}`;
  fs.mkdirSync(path.join(work, top));
  fs.writeFileSync(path.join(work, top, binaryName(KEY)), 'fake binary\n');
  const file = path.join(work, archiveName(VERSION, KEY));
  // bsdtar picks zip from the .zip suffix with -a.
  execFileSync(TAR, process.platform === 'win32' ? ['-a', '-cf', file, top] : ['-czf', file, top], { cwd: work });
  archive = fs.readFileSync(file);

  server = http.createServer((req, res) => {
    requests += 1;
    if (req.url === `/v${VERSION}/${archiveName(VERSION, KEY)}`) {
      res.end(archive);
    } else {
      res.statusCode = 404;
      res.end();
    }
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  base = `http://127.0.0.1:${server.address().port}`;
});

after(() => {
  server.close();
  fs.rmSync(work, { recursive: true, force: true });
});

function env(name) {
  return { MOCKINGBIRD_DOWNLOAD_URL: base, MOCKINGBIRD_CACHE_DIR: path.join(work, name) };
}

test('the asset URL follows the release', () => {
  assert.equal(
    assetUrl('0.1.0', 'darwin-arm64', {}),
    'https://github.com/RJ-s-Org/Mockingbird/releases/download/v0.1.0/mockingbird-0.1.0-aarch64-apple-darwin.tar.gz',
  );
  assert.equal(
    assetUrl('0.1.0', 'win32-x64', { MOCKINGBIRD_DOWNLOAD_URL: 'https://mirror.example/mb/' }),
    'https://mirror.example/mb/v0.1.0/mockingbird-0.1.0-x86_64-pc-windows-msvc.zip',
  );
});

test('the cache follows each platform convention', () => {
  assert.equal(cacheDir({ MOCKINGBIRD_CACHE_DIR: '/c' }, 'linux', '/h'), '/c');
  assert.equal(cacheDir({}, 'linux', '/h'), path.join('/h', '.cache', 'mockingbird'));
  assert.equal(cacheDir({ XDG_CACHE_HOME: '/x' }, 'linux', '/h'), path.join('/x', 'mockingbird'));
  assert.equal(cacheDir({}, 'darwin', '/h'), path.join('/h', 'Library', 'Caches', 'mockingbird'));
  assert.equal(cacheDir({ LOCALAPPDATA: 'C:\\L' }, 'win32', '/h'), path.join('C:\\L', 'mockingbird', 'cache'));
});

test('downloads, verifies, caches and reuses the binary', async () => {
  const e = env('ok');
  const before = requests;
  const opts = { version: VERSION, key: KEY, checksums: { [TARGET]: sha256(archive) }, env: e, log: () => {} };
  const bin = await downloadBinary(opts);
  assert.equal(bin, path.join(e.MOCKINGBIRD_CACHE_DIR, 'npm', VERSION, TARGET, binaryName(KEY)));
  assert.equal(fs.readFileSync(bin, 'utf8'), 'fake binary\n');
  if (process.platform !== 'win32') assert.ok(fs.statSync(bin).mode & 0o100, 'the binary is executable');
  assert.equal(await downloadBinary(opts), bin);
  assert.equal(requests - before, 1, 'the second call uses the cache');
  // Nothing but the binary is left behind.
  assert.deepEqual(fs.readdirSync(path.dirname(bin)), [binaryName(KEY)]);
});

test('a checksum mismatch fails and caches nothing', async () => {
  const e = env('bad');
  await assert.rejects(
    downloadBinary({ version: VERSION, key: KEY, checksums: { [TARGET]: '0'.repeat(64) }, env: e, log: () => {} }),
    /checksum mismatch/,
  );
  assert.ok(!fs.existsSync(path.join(e.MOCKINGBIRD_CACHE_DIR, 'npm', VERSION, TARGET, binaryName(KEY))));
});

test('an HTTP error fails with the URL', async () => {
  await assert.rejects(
    downloadBinary({ version: '0.0.1', key: KEY, checksums: { [TARGET]: 'x' }, env: env('404'), log: () => {} }),
    /cannot download http:\/\/127\.0\.0\.1:\d+\/v0\.0\.1\/.* HTTP 404/,
  );
});

test('without a checksum nothing is downloaded', async () => {
  const before = requests;
  await assert.rejects(
    downloadBinary({ version: VERSION, key: KEY, checksums: null, env: env('none'), log: () => {} }),
    /no checksum/,
  );
  assert.equal(requests, before);
});

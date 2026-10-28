'use strict';

const assert = require('node:assert/strict');
const path = require('node:path');
const { spawn, spawnSync } = require('node:child_process');
const { test } = require('node:test');

const { targetKey, platformPackage, binaryName, archiveName, TARGETS } = require('../lib/targets');
const { LauncherError, findBinary } = require('../lib/launcher');

const BIN = path.join(__dirname, '..', 'bin', 'mockingbird.js');
const notFound = () => {
  const err = new Error('Cannot find module');
  err.code = 'MODULE_NOT_FOUND';
  throw err;
};
const noDownload = () => {
  throw new Error('download must not be called');
};

test('every release target resolves to itself', () => {
  for (const [key, { os, cpu }] of Object.entries(TARGETS)) {
    assert.equal(targetKey(os, cpu), key);
  }
});

test('Windows on Arm uses the x64 build; other platforms have none', () => {
  assert.equal(targetKey('win32', 'arm64'), 'win32-x64');
  assert.equal(targetKey('freebsd', 'x64'), null);
  assert.equal(targetKey('linux', 'ia32'), null);
  assert.equal(targetKey('darwin', 'ppc64'), null);
});

test('names follow the release assets', () => {
  assert.equal(platformPackage('@scope/cli', 'linux-x64'), '@scope/cli-linux-x64');
  assert.equal(binaryName('win32-x64'), 'mockingbird.exe');
  assert.equal(binaryName('darwin-arm64'), 'mockingbird');
  assert.equal(archiveName('1.2.3', 'linux-arm64'), 'mockingbird-1.2.3-aarch64-unknown-linux-musl.tar.gz');
  assert.equal(archiveName('1.2.3', 'win32-x64'), 'mockingbird-1.2.3-x86_64-pc-windows-msvc.zip');
});

test('the platform package is used when it is installed', async () => {
  const asked = [];
  const found = await findBinary({
    platform: 'darwin',
    arch: 'arm64',
    env: {},
    name: '@scope/cli',
    resolve: (id) => {
      asked.push(id);
      return `/nm/${id}`;
    },
    download: noDownload,
  });
  assert.deepEqual(asked, ['@scope/cli-darwin-arm64/bin/mockingbird']);
  assert.equal(found, '/nm/@scope/cli-darwin-arm64/bin/mockingbird');
});

test('MOCKINGBIRD_BINARY wins over everything', async () => {
  const found = await findBinary({
    platform: 'aix',
    arch: 'ppc64',
    env: { MOCKINGBIRD_BINARY: '/opt/mb' },
    resolve: notFound,
    download: noDownload,
  });
  assert.equal(found, '/opt/mb');
});

test('an unsupported platform fails with the supported list', async () => {
  await assert.rejects(
    findBinary({ platform: 'freebsd', arch: 'x64', env: {}, name: '@scope/cli', resolve: notFound, download: noDownload }),
    (err) => {
      assert.ok(err instanceof LauncherError);
      assert.match(err.message, /no mockingbird binary for freebsd-x64/);
      assert.match(err.message, /linux-x64/);
      return true;
    },
  );
});

test('a missing platform package falls back to the download', async () => {
  let args;
  const found = await findBinary({
    platform: 'linux',
    arch: 'x64',
    env: {},
    name: '@scope/cli',
    version: '1.2.3',
    checksums: { 'x86_64-unknown-linux-musl': 'abc' },
    resolve: notFound,
    download: async (a) => {
      args = a;
      return '/cache/mockingbird';
    },
  });
  assert.equal(found, '/cache/mockingbird');
  assert.equal(args.version, '1.2.3');
  assert.equal(args.key, 'linux-x64');
  assert.deepEqual(args.checksums, { 'x86_64-unknown-linux-musl': 'abc' });
});

test('a missing platform package and a failed download explain both', async () => {
  await assert.rejects(
    findBinary({
      platform: 'linux',
      arch: 'arm64',
      env: {},
      name: '@scope/cli',
      version: '1.2.3',
      resolve: notFound,
      download: async () => {
        throw new Error('HTTP 404');
      },
    }),
    (err) => {
      assert.ok(err instanceof LauncherError);
      assert.match(err.message, /@scope\/cli-linux-arm64@1\.2\.3 is not installed/);
      assert.match(err.message, /--omit=optional/);
      assert.match(err.message, /HTTP 404/);
      return true;
    },
  );
});

test('MOCKINGBIRD_NO_DOWNLOAD turns the fallback off', async () => {
  await assert.rejects(
    findBinary({
      platform: 'win32',
      arch: 'x64',
      env: { MOCKINGBIRD_NO_DOWNLOAD: '1' },
      name: '@scope/cli',
      resolve: notFound,
      download: noDownload,
    }),
    /cli-win32-x64@.* is not installed[\s\S]*MOCKINGBIRD_NO_DOWNLOAD/,
  );
});

test('other resolution errors are not swallowed', async () => {
  await assert.rejects(
    findBinary({
      platform: 'linux',
      arch: 'x64',
      env: {},
      resolve: () => {
        throw new Error('EACCES');
      },
      download: noDownload,
    }),
    /EACCES/,
  );
});

// The launcher end to end, with Node itself standing in for the binary.
function launch(args, env = {}) {
  return spawnSync(process.execPath, [BIN, ...args], {
    env: { ...process.env, MOCKINGBIRD_BINARY: process.execPath, ...env },
    encoding: 'utf8',
  });
}

test('arguments and stdio pass through, and the exit code comes back', () => {
  const r = launch(['-e', 'console.log(process.argv.slice(1).join(" ")); process.exit(7)', 'run', '--', 'a b']);
  assert.equal(r.status, 7);
  assert.equal(r.stdout.trim(), 'run -- a b');
});

test('exit code 0 is kept', () => {
  assert.equal(launch(['-e', '']).status, 0);
});

test('a binary that cannot be started exits 127', () => {
  const r = launch([], { MOCKINGBIRD_BINARY: path.join(__dirname, 'no-such-binary') });
  assert.equal(r.status, 127);
  assert.match(r.stderr, /cannot run/);
});

test('a missing platform package exits 1 with an explanation', () => {
  const r = launch(['--version'], { MOCKINGBIRD_BINARY: '', MOCKINGBIRD_NO_DOWNLOAD: '1' });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /mockingbird: .*(is not installed|no mockingbird binary for)/);
});

const unix = process.platform !== 'win32';

function startChild(script) {
  const child = spawn(process.execPath, [BIN, '-e', script], {
    env: { ...process.env, MOCKINGBIRD_BINARY: process.execPath },
    stdio: ['ignore', 'pipe', 'inherit'],
  });
  let out = '';
  const ready = new Promise((resolve) => {
    child.stdout.on('data', (d) => {
      out += d;
      if (out.includes('ready')) resolve();
    });
  });
  const exited = new Promise((resolve) => child.on('exit', (code, signal) => resolve({ code, signal, out: () => out })));
  return { child, ready, exited };
}

test('signals are forwarded to the binary', { skip: !unix }, async () => {
  const { child, ready, exited } = startChild(
    "process.on('SIGTERM', () => { console.log('got SIGTERM'); process.exit(42); }); console.log('ready'); setInterval(() => {}, 1000);",
  );
  await ready;
  child.kill('SIGTERM');
  const { code, out } = await exited;
  assert.equal(code, 42);
  assert.match(out(), /got SIGTERM/);
});

test('a binary killed by a signal ends the launcher with that signal', { skip: !unix }, async () => {
  const { child, ready, exited } = startChild("console.log('ready'); setInterval(() => {}, 1000);");
  await ready;
  child.kill('SIGINT');
  const { code, signal } = await exited;
  assert.equal(code, null);
  assert.equal(signal, 'SIGINT');
});

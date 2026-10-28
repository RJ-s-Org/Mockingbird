'use strict';

// The release targets `.github/workflows/release.yml` builds, keyed by
// `${process.platform}-${process.arch}`. Each one is published as the
// platform package `<scope>/<key>`, in the main package's scope (the way
// esbuild publishes `@esbuild/darwin-arm64`).
//
// The Linux binaries are static musl builds, so one package serves glibc
// and musl systems alike and the packages carry no `libc` field.
const TARGETS = {
  'darwin-arm64': { os: 'darwin', cpu: 'arm64', target: 'aarch64-apple-darwin' },
  'darwin-x64': { os: 'darwin', cpu: 'x64', target: 'x86_64-apple-darwin' },
  'linux-arm64': { os: 'linux', cpu: 'arm64', target: 'aarch64-unknown-linux-musl' },
  'linux-x64': { os: 'linux', cpu: 'x64', target: 'x86_64-unknown-linux-musl' },
  'win32-x64': { os: 'win32', cpu: 'x64', target: 'x86_64-pc-windows-msvc' },
};

// Platforms without a build of their own that run another one. Windows on
// Arm runs the x64 build under emulation; npm skips the x64 package there
// (its `cpu` does not match), so the launcher downloads that build instead.
const ALIASES = {
  'win32-arm64': 'win32-x64',
};

/** The target key for a platform and architecture, or null when there is no build. */
function targetKey(platform, arch) {
  const key = `${platform}-${arch}`;
  if (TARGETS[key]) return key;
  return ALIASES[key] || null;
}

/** The platform package for a target key: `@scope/<key>` (or `<name>-<key>` unscoped). */
function platformPackage(mainName, key) {
  if (mainName.startsWith('@')) return `${mainName.split('/')[0]}/${key}`;
  return `${mainName}-${key}`;
}

/** The binary's file name on a target. */
function binaryName(key) {
  return TARGETS[key].os === 'win32' ? 'mockingbird.exe' : 'mockingbird';
}

/** The release asset (archive) for a version and target key. */
function archiveName(version, key) {
  const { os, target } = TARGETS[key];
  return `mockingbird-${version}-${target}.${os === 'win32' ? 'zip' : 'tar.gz'}`;
}

module.exports = { TARGETS, ALIASES, targetKey, platformPackage, binaryName, archiveName };

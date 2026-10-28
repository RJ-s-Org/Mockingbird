'use strict';

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');
const { TARGETS, targetKey, platformPackage, binaryName } = require('./targets');
const { downloadBinary } = require('./download');

const pkg = require('../package.json');

// Written by npm/scripts/build-packages.mjs; absent in a checkout.
function loadChecksums() {
  try {
    return require('./checksums.json');
  } catch {
    return null;
  }
}

class LauncherError extends Error {}

function supported() {
  return Object.keys(TARGETS).join(', ');
}

/**
 * Finds the mockingbird binary: `MOCKINGBIRD_BINARY`, then the platform
 * package, then a cached download of the release asset.
 */
async function findBinary({
  platform = process.platform,
  arch = process.arch,
  env = process.env,
  name = pkg.name,
  version = pkg.version,
  resolve = require.resolve,
  download = downloadBinary,
  checksums = loadChecksums(),
} = {}) {
  if (env.MOCKINGBIRD_BINARY) return env.MOCKINGBIRD_BINARY;

  const key = targetKey(platform, arch);
  if (!key) {
    throw new LauncherError(
      `${name} has no mockingbird binary for ${platform}-${arch}.\n` +
        `Supported platforms: ${supported()}.\n` +
        'Build from source instead: cargo install mockingbird-cli',
    );
  }

  const platformPkg = platformPackage(name, key);
  try {
    return resolve(`${platformPkg}/bin/${binaryName(key)}`);
  } catch (err) {
    if (err.code !== 'MODULE_NOT_FOUND') throw err;
  }

  const missing =
    `${platformPkg}@${version} is not installed. npm installs it with ${name} ` +
    'unless optional dependencies are skipped (--omit=optional, --no-optional) ' +
    'or the lockfile was written on another platform.';
  if (env.MOCKINGBIRD_NO_DOWNLOAD) {
    throw new LauncherError(`${missing}\nMOCKINGBIRD_NO_DOWNLOAD is set, so the release binary was not downloaded.`);
  }
  try {
    return await download({ version, key, checksums, env, platform });
  } catch (err) {
    throw new LauncherError(
      `${missing}\nDownloading the release binary instead failed: ${err.message}\n` +
        `Reinstall with optional dependencies (npm install ${name}), or set MOCKINGBIRD_BINARY to a mockingbird binary.`,
    );
  }
}

const FORWARDED = ['SIGINT', 'SIGTERM', 'SIGHUP', 'SIGQUIT', 'SIGBREAK'];

/**
 * Runs the binary with stdio inherited. Signals sent to this process are
 * passed on; resolves with the child's exit code, or its signal.
 */
function runBinary(binary, args) {
  return new Promise((resolve) => {
    const child = spawn(binary, args, { stdio: 'inherit' });
    const handlers = new Map();
    for (const sig of FORWARDED) {
      const handler = () => {
        if (child.pid && child.exitCode === null) child.kill(sig);
      };
      try {
        process.on(sig, handler);
        handlers.set(sig, handler);
      } catch {
        // Not a signal on this platform.
      }
    }
    const cleanup = () => {
      for (const [sig, handler] of handlers) process.off(sig, handler);
    };
    child.on('error', (err) => {
      cleanup();
      process.stderr.write(`mockingbird: cannot run ${binary}: ${err.message}\n`);
      resolve({ code: err.code === 'ENOENT' ? 127 : 126, signal: null });
    });
    child.on('exit', (code, signal) => {
      cleanup();
      resolve({ code, signal });
    });
  });
}

/** The launcher's entry point; returns the exit code (or ends with the child's signal). */
async function main(args) {
  let binary;
  try {
    binary = await findBinary();
  } catch (err) {
    if (!(err instanceof LauncherError)) throw err;
    process.stderr.write(`mockingbird: ${err.message}\n`);
    return 1;
  }
  // npm keeps file modes, but some tools that unpack packages do not.
  if (process.platform !== 'win32') {
    try {
      fs.accessSync(binary, fs.constants.X_OK);
    } catch {
      try {
        fs.chmodSync(binary, 0o755);
      } catch {
        // spawn reports it.
      }
    }
  }
  const { code, signal } = await runBinary(path.resolve(binary), args);
  if (signal) {
    // End the same way, so a shell or CI runner sees the signal.
    process.kill(process.pid, signal);
    return 128 + (os.constants.signals[signal] || 0);
  }
  return code ?? 1;
}

module.exports = { LauncherError, findBinary, runBinary, main };

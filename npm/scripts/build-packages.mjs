#!/usr/bin/env node
// Writes the npm packages for one version: the launcher package (npm/cli)
// and one platform package per release target, each holding that target's
// binary. Publishing is a separate step (release.yml, job npm-publish).
//
// From a release's assets (every target required; each archive is checked
// against its .sha256 file, and the launcher gets the checksums for its
// download fallback):
//   node npm/scripts/build-packages.mjs --version 0.1.0 --assets dist --out npm/dist
//
// From local binaries (only the targets given; no download fallback):
//   node npm/scripts/build-packages.mjs --version 0.1.0 --out npm/dist \
//     --binary linux-x64=target/debug/mockingbird
//
// Prints the package directories, launcher last, one per line.

import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';

const require = createRequire(import.meta.url);
const NPM = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const ROOT = path.resolve(NPM, '..');
const CLI = path.join(NPM, 'cli');
const { TARGETS, archiveName, binaryName, platformPackage } = require(path.join(CLI, 'lib', 'targets.js'));

function fail(msg) {
  console.error(`build-packages: ${msg}`);
  process.exit(1);
}

const { values } = parseArgs({
  options: {
    version: { type: 'string' },
    assets: { type: 'string' },
    binary: { type: 'string', multiple: true },
    out: { type: 'string' },
  },
});
const { version, assets, out } = values;
const binaries = values.binary ?? [];
if (!version || !out || (!assets) === (binaries.length === 0)) {
  fail('usage: build-packages.mjs --version <v> --out <dir> (--assets <dir> | --binary <platform-arch>=<path>...)');
}
if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) fail(`'${version}' is not a version (X.Y.Z[-pre])`);

const template = JSON.parse(fs.readFileSync(path.join(CLI, 'package.json'), 'utf8'));
const MAIN = template.name;
const LICENSES = ['LICENSE-MIT', 'LICENSE-APACHE'];

const sha256 = (file) => createHash('sha256').update(fs.readFileSync(file)).digest('hex');

// Each target's binary, and (from release assets) its archive's sha256.
const sources = new Map();
const checksums = {};
fs.mkdirSync(path.resolve(out, '..'), { recursive: true });
const scratch = fs.mkdtempSync(path.join(path.resolve(out, '..'), '.build-packages-'));
process.on('exit', () => fs.rmSync(scratch, { recursive: true, force: true }));

if (assets) {
  for (const key of Object.keys(TARGETS)) {
    const { target, os } = TARGETS[key];
    const archive = path.join(assets, archiveName(version, key));
    if (!fs.existsSync(archive)) fail(`missing release asset ${archive}`);
    const sidecar = `${archive}.sha256`;
    if (!fs.existsSync(sidecar)) fail(`missing ${sidecar}`);
    const expected = fs.readFileSync(sidecar, 'utf8').trim().split(/\s+/)[0];
    const actual = sha256(archive);
    if (actual !== expected) fail(`${archive}: sha256 ${actual} does not match ${sidecar} (${expected})`);
    checksums[target] = actual;

    const dir = path.join(scratch, key);
    fs.mkdirSync(dir);
    if (os === 'win32' && process.platform !== 'win32') {
      execFileSync('unzip', ['-q', archive, '-d', dir]);
    } else {
      execFileSync('tar', ['-xf', archive, '-C', dir]);
    }
    const bin = path.join(dir, `mockingbird-${version}-${target}`, binaryName(key));
    if (!fs.existsSync(bin)) fail(`${archive} has no mockingbird-${version}-${target}/${binaryName(key)}`);
    sources.set(key, bin);
  }
} else {
  for (const spec of binaries) {
    const [key, file] = spec.split(/=(.*)/s);
    if (!TARGETS[key]) fail(`unknown platform '${key}' (one of ${Object.keys(TARGETS).join(', ')})`);
    if (!file || !fs.existsSync(file)) fail(`no binary at '${file}'`);
    sources.set(key, file);
  }
}

const common = {
  version,
  homepage: template.homepage,
  bugs: template.bugs,
  repository: template.repository,
  license: template.license,
};

function copyLicenses(dir) {
  for (const file of LICENSES) fs.copyFileSync(path.join(ROOT, file), path.join(dir, file));
}

function writeJson(file, value) {
  fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
}

fs.rmSync(out, { recursive: true, force: true });
fs.mkdirSync(out, { recursive: true });
const written = [];

for (const [key, file] of sources) {
  const { os, cpu, target } = TARGETS[key];
  const name = platformPackage(MAIN, key);
  const dir = path.join(out, key);
  fs.mkdirSync(path.join(dir, 'bin'), { recursive: true });
  const bin = path.join(dir, 'bin', binaryName(key));
  fs.copyFileSync(file, bin);
  fs.chmodSync(bin, 0o755);
  copyLicenses(dir);
  fs.writeFileSync(
    path.join(dir, 'README.md'),
    `# ${name}\n\nThe \`mockingbird\` binary for ${os} ${cpu} (\`${target}\`), version ${version}.\n\n` +
      `Install [\`${MAIN}\`](https://www.npmjs.com/package/${MAIN}) instead; it picks this package on ${os} ${cpu}.\n`,
  );
  writeJson(path.join(dir, 'package.json'), {
    name,
    description: `The mockingbird binary for ${os} ${cpu}; install ${MAIN} instead`,
    ...common,
    repository: { ...common.repository, directory: 'npm' },
    os: [os],
    cpu: [cpu],
    files: ['bin', ...LICENSES],
    // Yarn PnP: the binary has to exist on disk.
    preferUnplugged: true,
  });
  written.push(dir);
}

const mainDir = path.join(out, 'cli');
fs.cpSync(path.join(CLI, 'bin'), path.join(mainDir, 'bin'), { recursive: true });
fs.cpSync(path.join(CLI, 'lib'), path.join(mainDir, 'lib'), { recursive: true });
fs.copyFileSync(path.join(CLI, 'README.md'), path.join(mainDir, 'README.md'));
copyLicenses(mainDir);
if (assets) writeJson(path.join(mainDir, 'lib', 'checksums.json'), checksums);
const { scripts: _scripts, ...manifest } = template;
writeJson(path.join(mainDir, 'package.json'), {
  ...manifest,
  version,
  optionalDependencies: Object.fromEntries(
    [...sources.keys()].sort().map((key) => [platformPackage(MAIN, key), version]),
  ),
});
written.push(mainDir);

for (const dir of written) console.log(dir);

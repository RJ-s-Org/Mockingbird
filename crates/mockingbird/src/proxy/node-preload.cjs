// Mockingbird's Node.js preload, written by `mockingbird proxy env` and
// loaded with `NODE_OPTIONS=--require <this file>`. It sends every
// `http`/`https` agent (the global ones and any an SDK creates itself) and
// undici's global dispatcher (`fetch`) through the proxy named by
// `HTTPS_PROXY`/`HTTP_PROXY`, honouring `NO_PROXY`, and trusts the local CA
// in `MOCKINGBIRD_CA_FILE` (also when a client passes its own `ca`). No
// package is needed. It does nothing when no proxy variable is set.
'use strict';

(function mockingbirdProxyPreload() {
  if (globalThis.__mockingbirdProxyPreload) return;
  globalThis.__mockingbirdProxyPreload = true;

  const env = process.env;
  const proxyText =
    env.HTTPS_PROXY || env.https_proxy || env.HTTP_PROXY || env.http_proxy || '';
  if (!proxyText) return;
  let proxy;
  try {
    proxy = new URL(proxyText);
  } catch {
    return;
  }
  if (proxy.protocol !== 'http:') return;
  const proxyHost = proxy.hostname.replace(/^\[|\]$/g, '');
  const proxyPort = Number(proxy.port) || 80;

  const fs = require('fs');
  const net = require('net');
  const tls = require('tls');
  const http = require('http');
  const https = require('https');

  let caPem = null;
  try {
    if (env.MOCKINGBIRD_CA_FILE) caPem = fs.readFileSync(env.MOCKINGBIRD_CA_FILE, 'utf8');
  } catch {
    caPem = null;
  }

  const noProxy = (env.NO_PROXY || env.no_proxy || '')
    .split(',')
    .map((s) => s.trim().toLowerCase())
    .filter(Boolean);

  function bypass(host) {
    host = String(host || '').toLowerCase().replace(/^\[|\]$/g, '');
    if (!host) return true;
    if (host === proxyHost) return true;
    for (const entry of noProxy) {
      if (entry === '*') return true;
      const bare = entry.replace(/:\d+$/, '').replace(/^\*?\./, '');
      if (host === bare || host.endsWith('.' + bare)) return true;
    }
    return false;
  }

  // Opens a TCP tunnel to host:port through the proxy (`CONNECT`).
  function tunnel(host, port, callback) {
    let done = false;
    const finish = (err, socket) => {
      if (done) return;
      done = true;
      callback(err, socket);
    };
    const socket = net.connect(proxyPort, proxyHost);
    const authority = host.includes(':') ? `[${host}]:${port}` : `${host}:${port}`;
    let buffered = Buffer.alloc(0);
    const onData = (chunk) => {
      buffered = Buffer.concat([buffered, chunk]);
      const end = buffered.indexOf('\r\n\r\n');
      if (end === -1) {
        if (buffered.length > 16384) {
          socket.destroy();
          finish(new Error('mockingbird proxy: oversized CONNECT response'));
        }
        return;
      }
      socket.removeListener('data', onData);
      socket.removeListener('error', onError);
      const status = buffered.subarray(0, buffered.indexOf('\r\n')).toString();
      if (!/^HTTP\/1\.[01] 2\d\d/.test(status)) {
        socket.destroy();
        finish(new Error(`mockingbird proxy: CONNECT ${authority} failed: ${status}`));
        return;
      }
      const rest = buffered.subarray(end + 4);
      if (rest.length) socket.unshift(rest);
      finish(null, socket);
    };
    const onError = (err) => finish(err);
    socket.on('data', onData);
    socket.on('error', onError);
    socket.once('connect', () => {
      socket.write(`CONNECT ${authority} HTTP/1.1\r\nHost: ${authority}\r\n\r\n`);
    });
  }

  function withCa(options) {
    if (!caPem || !options.ca) return options;
    const ca = Array.isArray(options.ca) ? options.ca.slice() : [options.ca];
    ca.push(caPem);
    return { ...options, ca };
  }

  // TLS over a tunnel, keeping the client's TLS options.
  function secure(raw, options, host) {
    const servername = options.servername || (net.isIP(host) ? undefined : host);
    return tls.connect({ ...withCa(options), socket: raw, servername });
  }

  // `Agent#createConnection` in callback form: Node's agent waits for the
  // callback when nothing is returned.
  function patchAgent(Agent, isTls) {
    const original = Agent.prototype.createConnection;
    Agent.prototype.createConnection = function createConnection(options, callback) {
      const host = options.host || options.hostname || 'localhost';
      const port = Number(options.port) || (isTls ? 443 : 80);
      if (bypass(host) || typeof callback !== 'function') {
        return original.call(this, options, callback);
      }
      tunnel(host, port, (err, raw) => {
        if (err) return callback(err);
        if (!isTls) return callback(null, raw);
        const socket = secure(raw, options, host);
        let settled = false;
        socket.once('secureConnect', () => {
          if (!settled) {
            settled = true;
            callback(null, socket);
          }
        });
        socket.once('error', (e) => {
          if (!settled) {
            settled = true;
            callback(e);
          }
        });
      });
      return undefined;
    };
  }
  patchAgent(http.Agent, false);
  patchAgent(https.Agent, true);
  // Node's own env-proxy support (`NODE_USE_ENV_PROXY`) would proxy the
  // global agents a second time: replace them with plain ones.
  try {
    http.globalAgent = new http.Agent({ keepAlive: true });
    https.globalAgent = new https.Agent({ keepAlive: true });
  } catch {
    // Read-only in some runtimes; the patched prototype still applies.
  }

  // undici (`fetch`): an Agent of the bundled undici with a connector that
  // tunnels through the proxy. Node creates the global dispatcher on the
  // first `fetch`, so a `data:` fetch (no network) makes it exist. undici 7
  // keeps it under `globalDispatcher.2` with a wrapper under `.1`; older
  // versions under `.1` only. The symbols are shared with the `undici`
  // package. Node's own env-proxy dispatcher (`NODE_USE_ENV_PROXY`) is
  // kept.
  const connect = (opts, callback) => {
    const host = opts.hostname || opts.host;
    const isTls = opts.protocol === 'https:';
    const port = Number(opts.port) || (isTls ? 443 : 80);
    const settle = (socket, ready) => {
      let settled = false;
      socket.once(ready, () => {
        if (!settled) {
          settled = true;
          callback(null, socket);
        }
      });
      socket.once('error', (e) => {
        if (!settled) {
          settled = true;
          callback(e);
        }
      });
    };
    const servername = opts.servername || (net.isIP(host) ? undefined : host);
    if (bypass(host)) {
      if (isTls) settle(tls.connect({ host, port, servername }), 'secureConnect');
      else settle(net.connect(port, host), 'connect');
      return;
    }
    tunnel(host, port, (err, raw) => {
      if (err) return callback(err);
      if (!isTls) return callback(null, raw);
      settle(tls.connect({ socket: raw, servername }), 'secureConnect');
    });
  };
  try {
    if (typeof globalThis.fetch === 'function') {
      const pending = globalThis.fetch('data:,');
      if (pending && typeof pending.catch === 'function') pending.catch(() => {});
    }
    const V1 = Symbol.for('undici.globalDispatcher.1');
    const V2 = Symbol.for('undici.globalDispatcher.2');
    const isAgent = (d) => d && d.constructor && d.constructor.name === 'Agent';
    if (isAgent(globalThis[V2])) {
      const agent = new globalThis[V2].constructor({ connect });
      const Wrapper = globalThis[V1] && globalThis[V1].constructor;
      globalThis[V2] = agent;
      if (Wrapper) globalThis[V1] = isAgent(globalThis[V1]) ? agent : new Wrapper(agent);
    } else if (isAgent(globalThis[V1])) {
      globalThis[V1] = new globalThis[V1].constructor({ connect });
    }
  } catch {
    // Leave fetch as it is (NODE_USE_ENV_PROXY may still cover it).
  }
})();

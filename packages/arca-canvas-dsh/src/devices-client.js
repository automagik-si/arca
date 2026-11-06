import { request as httpRequest } from 'node:http';
import { request as httpsRequest } from 'node:https';
import { isIP } from 'node:net';
import { isAbsolute } from 'node:path';
import { PROTOCOL_VERSION } from './arca-client.js';
import { readToken } from './token.js';

const READS = new Map([
  ['droid_list', 'droids'], ['desktop_list', 'desktops'],
  ['droidd_status', null], ['phone_status', null],
]);
const failure = (setting) => Object.assign(new Error('Device reader is unavailable.'), { setting });

export function validateDevicesUrl(value) {
  try {
    if (!/^https?:\/\//i.test(value) || /[\s\\]/.test(value)) return null;
    const url = new URL(value);
    if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || value.includes('#')) return null;
    const authority = /^https?:\/\/([^/?#]+)/i.exec(value)?.[1];
    if (!authority || authority.includes('@')) return null;
    if (url.protocol === 'https:') return url;
    // Check the literal authority, before URL normalizes legacy numeric addresses.
    const match = /^(?:\[([^\]]+)\]|([^:]+))(?::[0-9]+)?$/.exec(authority ?? '');
    const host = match?.[1] ?? match?.[2] ?? '';
    if (isIP(host) === 4) {
      const [a, b] = host.split('.').map(Number);
      if (a === 127 || a === 10 || (a === 172 && b >= 16 && b <= 31) ||
          (a === 192 && b === 168) || (a === 100 && b >= 64 && b <= 127)) return url;
    } else if (isIP(host) === 6 && (url.hostname === '[::1]' || /^\[fd[0-9a-f]{2}:/i.test(url.hostname))) return url;
  } catch { /* Refused settings never prevent the host from starting. */ }
  return null;
}


async function rpc(config, url, id, method, params, bearers, signal) {
  signal?.throwIfAborted();
  const token = await readToken(config.devicesTokenPath).catch(() => { throw failure('devicesTokenPath'); });
  signal?.throwIfAborted();
  bearers.add(token);
  const body = JSON.stringify({ jsonrpc: '2.0', id, method, params });
  return new Promise((resolve, reject) => {
    const fail = () => { req.destroy(); reject(failure()); };
    const req = (url.protocol === 'https:' ? httpsRequest : httpRequest)(url, {
      method: 'POST', signal,
      headers: {
        'Content-Type': 'application/json', Accept: 'application/json, text/event-stream',
        Authorization: `Bearer ${token}`, 'MCP-Protocol-Version': PROTOCOL_VERSION,
        'Content-Length': Buffer.byteLength(body),
      },
    }, (res) => {
      if (res.statusCode !== 200) { fail(); res.destroy(); return; }
      const chunks = [];
      let size = 0;
      res.on('data', (chunk) => {
        size += chunk.length;
        if (size > 4 * 1024 * 1024) { fail(); res.destroy(); }
        else chunks.push(chunk);
      });
      res.on('error', fail);
      res.on('end', () => {
        try {
          let data = Buffer.concat(chunks).toString('utf8');
          for (const bearer of bearers) data = data.replaceAll(bearer, '[redacted]');
          const value = JSON.parse(data);
          if (value?.jsonrpc !== '2.0' || value.id !== id || value.error ||
              !value.result || typeof value.result !== 'object' || value.result.isError) throw failure();
          resolve(value.result);
        } catch { fail(); }
      });
    });
    req.on('error', fail);
    req.end(body);
  });
}

export function createDevicesClient(config = {}) {
  if (!config.devicesUrl && !config.devicesTokenPath) return null;
  const url = validateDevicesUrl(config.devicesUrl ?? '');
  const rejected = !url ? 'devicesUrl'
    : !config.devicesTokenPath || !isAbsolute(config.devicesTokenPath) ? 'devicesTokenPath' : null;
  let nextId = 0;
  return {
    async call(name, args = {}, signal) {
      try {
        if (rejected) throw failure(rejected);
        if (!READS.has(name)) throw failure();
        const bearers = new Set();
        const initialized = await rpc(config, url, ++nextId, 'initialize', {
          protocolVersion: PROTOCOL_VERSION, capabilities: {},
          clientInfo: { name: 'arca-canvas', version: '0.1.0' },
        }, bearers, signal);
        if (initialized.protocolVersion !== PROTOCOL_VERSION) throw failure();
        const result = await rpc(config, url, ++nextId, 'tools/call', { name, arguments: args }, bearers, signal);
        const key = READS.get(name);
        if (result.structuredContent && typeof result.structuredContent === 'object') {
          const value = key ? result.structuredContent[key] : result.structuredContent;
          if (key ? !Array.isArray(value) : !value || Array.isArray(value)) throw failure();
          return value;
        }
        // Older servers prepend an untrusted-output notice to the JSON block.
        for (const item of result.content ?? []) {
          if (item.type !== 'text') continue;
          try {
            const value = JSON.parse(item.text);
            if (key ? Array.isArray(value) : value && typeof value === 'object' && !Array.isArray(value)) return value;
          } catch { /* A notice or summary is not JSON data. */ }
        }
        throw failure();
      } catch (error) { throw failure(error.setting); }
    },
  };
}

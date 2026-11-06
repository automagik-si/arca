import { request as httpRequest } from 'node:http';
import { request as httpsRequest } from 'node:https';
import { PROTOCOL_VERSION } from './arca-client.js';
import { readToken } from './token.js';

const READS = new Map([
  ['droid_list', 'droids'], ['desktop_list', 'desktops'],
  ['droidd_status', null], ['phone_status', null],
]);
const failure = () => new Error('Device reader is unavailable.');

async function rpc(config, id, method, params, bearers) {
  const token = await readToken(config.droiddTokenPath);
  bearers.add(token);
  const url = new URL(config.droiddUrl);
  if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || url.hash) throw failure();
  const body = JSON.stringify({ jsonrpc: '2.0', id, method, params });
  return new Promise((resolve, reject) => {
    const fail = () => reject(failure());
    const req = (url.protocol === 'https:' ? httpsRequest : httpRequest)(url, {
      method: 'POST', signal: AbortSignal.timeout(5000),
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

export function createDroiddClient(config = {}) {
  if (!config.droiddUrl || !config.droiddTokenPath) return null;
  let nextId = 0;
  return {
    async call(name, args = {}) {
      try {
        if (!READS.has(name)) throw failure();
        const bearers = new Set();
        const initialized = await rpc(config, ++nextId, 'initialize', {
          protocolVersion: PROTOCOL_VERSION, capabilities: {},
          clientInfo: { name: 'arca-canvas', version: '0.1.0' },
        }, bearers);
        if (initialized.protocolVersion !== PROTOCOL_VERSION) throw failure();
        const result = await rpc(config, ++nextId, 'tools/call', { name, arguments: args }, bearers);
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
      } catch { throw failure(); }
    },
  };
}

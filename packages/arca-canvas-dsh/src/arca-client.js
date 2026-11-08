import { request } from 'node:http';
import { readToken } from './token.js';

export const PROTOCOL_VERSION = '2025-11-25';
const MAX_BODY = 4 * 1024 * 1024;
const messages = {
  token: 'Viewer token requires a regular 0400 file with a valid bearer.',
  unavailable: 'Arca socket is unavailable or timed out.',
  unauthorized: 'Arca refused the viewer token (401).',
  forbidden: 'Arca refused the request (403).',
  http: 'Arca returned an unsuccessful HTTP status.',
  json: 'Arca returned malformed JSON or an invalid MCP response.',
  rpc: 'Arca could not complete the MCP request.',
  protocol: 'Arca returned an unsupported MCP protocol version.',
};

export class ArcaError extends Error {
  constructor(code) {
    super(messages[code] ?? 'Arca request failed.');
    this.code = code;
  }
}

export function snapshotError(error) {
  return error instanceof ArcaError && Object.hasOwn(messages, error.code)
    ? messages[error.code] : 'Arca request failed.';
}

async function rpc(config, id, method, params, signal) {
  // Authentication is refreshed for every HTTP call, including initialize.
  const token = await readToken(config.viewerTokenPath).catch(() => { throw new ArcaError('token'); });
  const body = JSON.stringify({ jsonrpc: '2.0', id, method, params });
  return new Promise((resolve, reject) => {
    const fail = (code) => reject(new ArcaError(code));
    const req = request({
      socketPath: config.socketPath,
      path: '/mcp',
      method: 'POST',
      signal: signal ? AbortSignal.any([AbortSignal.timeout(5000), signal]) : AbortSignal.timeout(5000),
      headers: {
        'Content-Type': 'application/json',
        Accept: 'application/json',
        Authorization: `Bearer ${token}`,
        'MCP-Protocol-Version': PROTOCOL_VERSION,
        'Content-Length': Buffer.byteLength(body),
      },
    }, (res) => {
      if (res.statusCode !== 200) {
        fail(res.statusCode === 401 ? 'unauthorized' : res.statusCode === 403 ? 'forbidden' : 'http');
        res.destroy();
        return;
      }
      const chunks = [];
      let size = 0;
      res.on('data', (chunk) => {
        size += chunk.length;
        if (size > MAX_BODY) {
          fail('json');
          res.destroy();
        } else chunks.push(chunk);
      });
      res.on('error', () => fail('unavailable'));
      res.on('end', () => {
        try {
          const parsed = JSON.parse(Buffer.concat(chunks).toString('utf8'));
          // Even an upstream echo must not publish the bearer in successful data.
          const value = JSON.parse(JSON.stringify(parsed).replaceAll(token, '[redacted]'));
          if (value?.jsonrpc !== '2.0' || value.id !== id) throw new ArcaError('json');
          if (value.error || value.result?.isError) throw new ArcaError('rpc');
          if (!value.result || typeof value.result !== 'object') throw new ArcaError('json');
          resolve(value.result);
        } catch (error) {
          fail(error instanceof ArcaError ? error.code : 'json');
        }
      });
    });
    req.on('error', () => fail('unavailable'));
    req.end(body);
  });
}

export function createArcaClient(config) {
  let nextId = 0;
  return {
    async call(name, args = {}, signal) {
      const initialized = await rpc(config, ++nextId, 'initialize', {
        protocolVersion: PROTOCOL_VERSION,
        capabilities: {},
        clientInfo: { name: 'arca-canvas', version: '0.1.0' },
      }, signal);
      if (initialized.protocolVersion !== PROTOCOL_VERSION) throw new ArcaError('protocol');
      const result = await rpc(config, ++nextId, 'tools/call', { name, arguments: args }, signal);
      if (result.structuredContent && typeof result.structuredContent === 'object') {
        return result.structuredContent;
      }
      try {
        const text = result.content?.find((item) => item.type === 'text')?.text;
        const value = JSON.parse(text);
        if (!value || typeof value !== 'object') throw new ArcaError('json');
        return value;
      } catch {
        throw new ArcaError('json');
      }
    },
  };
}

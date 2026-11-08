import { readFileSync } from 'node:fs';
import { hostname } from 'node:os';
import z from '@deepseek-ai/schemastery';
import { approvalUrl } from './src/approval.js';
import { createDevicesClient } from './src/devices-client.js';
import { createArcaClient } from './src/arca-client.js';
import { createSnapshotCache } from './src/snapshot.js';

export const name = 'arca-canvas';
// Optional sidebar integration belongs to the client; stock DSH needs only routes.
export const inject = ['webServer'];
const hostConfig = z.object({
  name: z.string().required(),
  socketPath: z.string().pattern(/^\//).required(),
  viewerTokenPath: z.string().pattern(/^\//).required(),
  devicesUrl: z.string(),
  devicesTokenPath: z.string(),
});
export const Config = z.transform(z.object({
  hosts: z.array(hostConfig).default(undefined),
  socketPath: z.string().pattern(/^\//),
  viewerTokenPath: z.string().pattern(/^\//),
  pollMs: z.number().min(100).max(2147483647).step(1).default(5000),
  nodeName: z.string(),
  approvalUrl: z.transform(z.string().default(''), (value) => {
    if (value && !approvalUrl(value)) throw new z.ValidationError('approvalUrl must be an absolute HTTP(S) URL without credentials', { path: ['approvalUrl'] });
    return value;
  }),
  devicesUrl: z.string().default(''),
  devicesTokenPath: z.string().default(''),
}), (config) => {
  const invalid = (message) => { throw new z.ValidationError(message, { path: ['hosts'] }); };
  if (config.hosts !== undefined) {
    if (!config.hosts.length) invalid('hosts must contain at least one host');
    const names = new Set();
    for (const host of config.hosts) {
      if (!host.name.trim() || names.has(host.name)) invalid('Host names must be non-empty and unique');
      names.add(host.name);
    }
  } else if (!config.socketPath || !config.viewerTokenPath) {
    invalid('socketPath and viewerTokenPath are required without hosts');
  }
  return config;
});

const page = `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<meta name="color-scheme" content="light dark"><title>Arca canvas</title>
<link rel="stylesheet" href="arca-canvas/page.css"></head>
<body><main id="arca-root" data-arca-base="/arca-canvas"></main><script src="arca-canvas/page.js" defer></script></body></html>`;
const assets = new Map([
  ['/arca-canvas/page.js', ['text/javascript', readFileSync(new URL('./page.js', import.meta.url))]],
  ['/arca-canvas/page.css', ['text/css', readFileSync(new URL('./page.css', import.meta.url))]],
]);

export function apply(ctx, config) {
  if (config.approvalUrl && !approvalUrl(config.approvalUrl)) throw new Error('approvalUrl must be an absolute HTTP(S) URL without credentials');
  const hosts = config.hosts ?? [{ ...config, name: (config.nodeName ?? hostname()).trim() || 'Machine' }];
  const caches = hosts.map((host, index) => {
    const settings = {
      ...host,
      devicesUrl: host.devicesUrl ?? (index === 0 ? config.devicesUrl : ''),
      devicesTokenPath: host.devicesTokenPath ?? (index === 0 ? config.devicesTokenPath : ''),
    };
    const cache = createSnapshotCache(createArcaClient(settings), config.pollMs,
      { nodeName: host.name }, createDevicesClient(settings));
    return { name: host.name, cache };
  });
  ctx.effect(() => () => caches.forEach(({ cache }) => cache.stop()));
  ctx.effect(() => ctx.webServer.register({
    kind: 'prefix',
    path: '/arca-canvas',
    handler(req, res) {
      const path = new URL(req.url ?? '/', 'http://example.invalid').pathname;
      res.setHeader('Cache-Control', 'no-store');
      res.setHeader('X-Content-Type-Options', 'nosniff');
      if (path !== '/arca-canvas' && path !== '/arca-canvas/snapshot' && !assets.has(path)) {
        res.writeHead(404).end();
      } else if (req.method !== 'GET') {
        res.writeHead(405, { Allow: 'GET' }).end();
      } else if (assets.has(path)) {
        const [type, body] = assets.get(path);
        res.writeHead(200, { 'Content-Type': `${type}; charset=utf-8` });
        res.end(body);
      } else if (path === '/arca-canvas/snapshot') {
        res.writeHead(200, { 'Content-Type': 'application/json; charset=utf-8' });
        res.end(JSON.stringify({
          at: new Date().toISOString(),
          approvalUrl: approvalUrl(config.approvalUrl),
          hosts: caches.map(({ name, cache }) => {
            const { approvalUrl: unused, ...snapshot } = cache.get();
            return { name, ...snapshot };
          }),
        }));
      } else {
        res.writeHead(200, {
          'Content-Type': 'text/html; charset=utf-8',
          'Content-Security-Policy': "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; frame-ancestors 'self'; base-uri 'none'",
        });
        res.end(page);
      }
    },
  }));
}

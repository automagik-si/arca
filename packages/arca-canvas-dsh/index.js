import { readFileSync } from 'node:fs';
import z from '@deepseek-ai/schemastery';
import { approvalUrl } from './src/approval.js';
import { createArcaClient } from './src/arca-client.js';
import { createSnapshotCache } from './src/snapshot.js';

export const name = 'arca-canvas';
// Optional sidebar integration belongs to the client; stock DSH needs only routes.
export const inject = ['webServer'];
export const Config = z.object({
  socketPath: z.string().pattern(/^\//).required(),
  viewerTokenPath: z.string().pattern(/^\//).required(),
  pollMs: z.number().min(100).max(2147483647).step(1).default(5000),
  approvalUrl: z.transform(z.string().default(''), (value) => {
    if (value && !approvalUrl(value)) throw new z.ValidationError('approvalUrl must be an absolute HTTP(S) URL without credentials', { path: ['approvalUrl'] });
    return value;
  }),
  droiddUrl: z.string().default(''),
  droiddTokenPath: z.string().default(''),
});

const page = `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<meta name="color-scheme" content="light dark"><title>Arca canvas</title>
<link rel="stylesheet" href="/arca-canvas/page.css"></head>
<body><main id="arca-root"></main><script src="/arca-canvas/page.js" defer></script></body></html>`;
const assets = new Map([
  ['/arca-canvas/page.js', ['text/javascript', readFileSync(new URL('./page.js', import.meta.url))]],
  ['/arca-canvas/page.css', ['text/css', readFileSync(new URL('./page.css', import.meta.url))]],
]);

export function apply(ctx, config) {
  if (config.approvalUrl && !approvalUrl(config.approvalUrl)) throw new Error('approvalUrl must be an absolute HTTP(S) URL without credentials');
  const cache = createSnapshotCache(createArcaClient(config), config.pollMs, { approvalUrl: approvalUrl(config.approvalUrl) });
  ctx.effect(() => () => cache.stop());
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
        res.end(JSON.stringify(cache.get()));
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

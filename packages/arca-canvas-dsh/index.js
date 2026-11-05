import z from '@deepseek-ai/schemastery';
import { createArcaClient } from './src/arca-client.js';
import { createSnapshotCache } from './src/snapshot.js';

export const name = 'arca-canvas';
// Optional sidebar integration belongs to the client; stock DSH needs only routes.
export const inject = ['webServer'];
export const Config = z.object({
  socketPath: z.string().pattern(/^\//).required(),
  viewerTokenPath: z.string().pattern(/^\//).required(),
  pollMs: z.number().min(100).max(2147483647).step(1).default(5000),
  droiddUrl: z.string().default(''),
  droiddTokenPath: z.string().default(''),
});

const page = `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<title>Arca canvas</title></head><body><main><h1>Arca canvas</h1>
<p>The read-only snapshot is available at <a href="/arca-canvas/snapshot">/arca-canvas/snapshot</a>.</p>
</main></body></html>`;

export function apply(ctx, config) {
  const cache = createSnapshotCache(createArcaClient(config), config.pollMs);
  ctx.effect(() => () => cache.stop());
  ctx.effect(() => ctx.webServer.register({
    kind: 'prefix',
    path: '/arca-canvas',
    handler(req, res) {
      const path = new URL(req.url ?? '/', 'http://example.invalid').pathname;
      res.setHeader('Cache-Control', 'no-store');
      res.setHeader('X-Content-Type-Options', 'nosniff');
      if (path !== '/arca-canvas' && path !== '/arca-canvas/snapshot') {
        res.writeHead(404).end();
      } else if (req.method !== 'GET') {
        res.writeHead(405, { Allow: 'GET' }).end();
      } else if (path === '/arca-canvas/snapshot') {
        res.writeHead(200, { 'Content-Type': 'application/json; charset=utf-8' });
        res.end(JSON.stringify(cache.get()));
      } else {
        res.writeHead(200, {
          'Content-Type': 'text/html; charset=utf-8',
          'Content-Security-Policy': "default-src 'none'; frame-ancestors 'self'",
        });
        res.end(page);
      }
    },
  }));
}

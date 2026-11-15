import { readFileSync } from 'node:fs';
import { approvalUrl } from './src/approval.js';
import { createDevicesClient } from './src/devices-client.js';
import { createSnapshotCache } from './src/snapshot.js';
import { createDoctorChat, stateFilePath } from './src/chat.js';
import { Config, createHostReaders, doctorOptions } from './src/doctor-session.js';

export const name = 'arca-canvas';
// Optional sidebar integration belongs to the client; stock DSH needs only routes.
export const inject = ['webServer'];
// The config schema is the Doctor's shared hosts config, so the terminal entry (doctor.js) takes the same block.
export { Config };

const page = `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<meta name="color-scheme" content="light dark"><title>Automagik Arca canvas</title>
<link rel="stylesheet" href="arca-canvas/page.css"></head>
<body><main id="arca-root" data-arca-base="/arca-canvas"></main><script src="arca-canvas/page.js" defer></script></body></html>`;
const assets = new Map([
  ['/arca-canvas/page.js', ['text/javascript', readFileSync(new URL('./page.js', import.meta.url))]],
  ['/arca-canvas/page.css', ['text/css', readFileSync(new URL('./page.css', import.meta.url))]],
]);

export function apply(ctx, config) {
  if (config.approvalUrl && !approvalUrl(config.approvalUrl)) throw new Error('approvalUrl must be an absolute HTTP(S) URL without credentials');
  const readers = createHostReaders(config);
  const caches = readers.map(({ name, address, client, settings }) => ({
    name,
    address,
    cache: createSnapshotCache(client, config.pollMs, { nodeName: name }, createDevicesClient(settings)),
  }));
  const chat = config.chatEnabled ? createDoctorChat({
    ...doctorOptions(config, readers),
    resolveDsh() {
      const agents = ctx.get('agents');
      const tools = ctx.get('tools');
      if (!agents || !tools) return null;
      return { agents, tools, systemPrompt: ctx.get('systemPrompt'), sessionTitle: ctx.get('sessionTitle') };
    },
    statePath: stateFilePath(process.env, config.chatStatePath),
    dailyTokenBudget: config.dailyTokenBudget,
    log(message) { ctx.logger?.info?.(message); },
  }) : null;
  ctx.effect(() => () => caches.forEach(({ cache }) => cache.stop()));
  ctx.effect(() => () => { void chat?.close(); });
  ctx.effect(() => ctx.webServer.register({
    kind: 'prefix',
    path: '/arca-canvas',
    handler(req, res) {
      const path = new URL(req.url ?? '/', 'http://example.invalid').pathname;
      res.setHeader('Cache-Control', 'no-store');
      res.setHeader('X-Content-Type-Options', 'nosniff');
      if (path === '/arca-canvas/chat') {
        if (!chat) {
          const body = JSON.stringify({ error: 'chat-unavailable' });
          res.writeHead(503, { 'Content-Type': 'application/json; charset=utf-8' }).end(body);
          return;
        }
        void chat.handle(req, res);
        return;
      }
      if (path === '/arca-canvas/chat/history') {
        if (req.method !== 'GET') {
          res.writeHead(405, { Allow: 'GET' }).end();
          return;
        }
        if (!chat) {
          const body = JSON.stringify({ error: 'chat-unavailable' });
          res.writeHead(503, { 'Content-Type': 'application/json; charset=utf-8' }).end(body);
          return;
        }
        void chat.history(res).catch(() => {
          if (!res.headersSent) res.writeHead(500, { 'Content-Type': 'application/json; charset=utf-8' }).end('{"error":"history-unavailable"}');
          else res.end();
        });
        return;
      }
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
          pageHost: (readers[0]?.name ?? '').trim(),
          projects: (config.projects ?? []).map(({ name, domain, members }) => ({
            name,
            domain: domain ?? '',
            members: members.map(({ host, incusProject, instance }) => ({ host, incusProject, instance })),
          })),
          defaultProject: (config.defaultProject ?? '').trim(),
          hosts: caches.map(({ name, address, cache }) => {
            const { approvalUrl: unused, ...snapshot } = cache.get();
            return { name, address, ...snapshot };
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

import { readFileSync } from 'node:fs';
import { hostname } from 'node:os';
import z from '@deepseek-ai/schemastery';
import { approvalUrl } from './src/approval.js';
import { createDevicesClient } from './src/devices-client.js';
import { createArcaClient } from './src/arca-client.js';
import { createSnapshotCache } from './src/snapshot.js';
import { createDoctorChat, stateFilePath } from './src/chat.js';

export const name = 'arca-canvas';
// Optional sidebar integration belongs to the client; stock DSH needs only routes.
export const inject = ['webServer'];
const hostConfig = z.object({
  name: z.string().required(),
  socketPath: z.string().pattern(/^\//).required(),
  viewerTokenPath: z.string().pattern(/^\//).required(),
  address: z.string().default(''),
  devicesUrl: z.string(),
  devicesTokenPath: z.string(),
});
const memberConfig = z.object({
  host: z.string().required(),
  incusProject: z.string().required(),
  instance: z.string().required(),
});
const projectConfig = z.object({
  name: z.string().required(),
  domain: z.string().default(''),
  members: z.array(memberConfig).required(),
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
  address: z.string().default(''),
  devicesUrl: z.string().default(''),
  devicesTokenPath: z.string().default(''),
  projects: z.array(projectConfig).default(undefined),
  defaultProject: z.string().default(''),
  chatEnabled: z.boolean().default(true),
  chatStatePath: z.string().default(''),
  dailyTokenBudget: z.number().min(1).max(1000000000).step(1).default(1000000),
}), (config) => {
  const invalid = (message) => { throw new z.ValidationError(message, { path: ['hosts'] }); };
  const invalidProjects = (message) => { throw new z.ValidationError(message, { path: ['projects'] }); };
  const address = (value) => (value ?? '').trim();
  const validAddress = (value) => !value || (/^[A-Za-z0-9_.:-]+$/.test(value)
    && !value.startsWith(':') && !value.endsWith(':')
    && !/^[.-]/.test(value) && !/[.-]$/.test(value)
    && !/^[^:]+:\d+$/.test(value));
  let hostNames;
  if (config.hosts !== undefined) {
    if (!config.hosts.length) invalid('hosts must contain at least one host');
    const names = new Set();
    for (const host of config.hosts) {
      if (!host.name.trim() || names.has(host.name)) invalid('Host names must be non-empty and unique');
      host.address = address(host.address);
      if (!validAddress(host.address)) invalid(`Host ${host.name} address must be a bare host name or IP address`);
      names.add(host.name);
    }
    hostNames = names;
  } else if (!config.socketPath || !config.viewerTokenPath) {
    invalid('socketPath and viewerTokenPath are required without hosts');
  } else {
    hostNames = new Set([((config.nodeName ?? hostname()).trim() || 'Machine')]);
  }
  if (!validAddress(address(config.address))) {
    throw new z.ValidationError('address must be a bare host name or IP address', { path: ['address'] });
  }
  config.address = address(config.address);
  if (config.chatStatePath && !config.chatStatePath.startsWith('/')) {
    throw new z.ValidationError('chatStatePath must be absolute', { path: ['chatStatePath'] });
  }
  if (config.projects !== undefined) {
    if (!config.projects.length) invalidProjects('projects must contain at least one project');
    const names = new Set();
    const memberships = new Set();
    for (const project of config.projects) {
      project.name = project.name.trim();
      project.domain = (project.domain ?? '').trim();
      if (!project.name || names.has(project.name)) invalidProjects('Project names must be non-empty and unique');
      names.add(project.name);
      if (!project.members.length) invalidProjects(`Project ${project.name} must contain at least one member`);
      for (const member of project.members) {
        member.host = member.host.trim();
        member.incusProject = member.incusProject.trim();
        member.instance = member.instance.trim();
        if (!member.host || !member.incusProject || !member.instance) invalidProjects('Project members require a non-empty host, incusProject and instance');
        if (hostNames && !hostNames.has(member.host)) invalidProjects(`Project ${project.name} names an unknown host`);
        const key = JSON.stringify([member.host, member.incusProject, member.instance]);
        if (memberships.has(key)) invalidProjects('An instance can belong to only one project');
        memberships.add(key);
      }
    }
    config.defaultProject = (config.defaultProject ?? '').trim();
    if (config.defaultProject && !names.has(config.defaultProject)) invalidProjects('defaultProject must name a configured project');
  } else if ((config.defaultProject ?? '').trim()) {
    invalidProjects('defaultProject requires projects');
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
  const readers = hosts.map((host, index) => {
    const settings = {
      ...host,
      address: (host.address ?? (index === 0 ? config.address : '')).trim(),
      devicesUrl: host.devicesUrl ?? (index === 0 ? config.devicesUrl : ''),
      devicesTokenPath: host.devicesTokenPath ?? (index === 0 ? config.devicesTokenPath : ''),
    };
    return { name: host.name, address: settings.address, client: createArcaClient(settings), settings };
  });
  const caches = readers.map(({ name, address, client, settings }) => ({
    name,
    address,
    cache: createSnapshotCache(client, config.pollMs, { nodeName: name }, createDevicesClient(settings)),
  }));
  const chat = config.chatEnabled ? createDoctorChat({
    hosts: readers.map(({ name, address }) => ({ name, address })),
    proposalHost: readers[0]?.name ?? '',
    approvalUrl: approvalUrl(config.approvalUrl),
    clientFor(name) {
      return readers.find((reader) => reader.name === name)?.client ?? null;
    },
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

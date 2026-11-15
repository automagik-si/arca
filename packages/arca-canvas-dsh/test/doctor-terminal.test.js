// The Doctor in a terminal (dsh-tui) must be the canvas Doctor: the same prompt, the same tools and the same guard.
// These tests open the web chat's Doctor session and the terminal entry's Doctor session from one config and
// compare what each one hands DSH, byte for byte.
import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import yaml from 'js-yaml';
import * as canvas from '../index.js';
import * as terminal from '../doctor.js';
import { ChatStore, createDoctorChat } from '../src/chat.js';
import {
  DOCTOR_MODEL, DOCTOR_PRESET, DOCTOR_TOOLS, PROPOSE_TOOL, READ_TOOLS, createDoctorTools, createHostReaders, doctorOptions,
} from '../src/doctor-session.js';

const NOW = Date.parse('2026-11-15T12:00:00Z');
const CONFIG = {
  hosts: [
    { name: 'alpha', address: 'host-a', socketPath: '/run/arca/a.sock', viewerTokenPath: '/etc/arca/a.token' },
    { name: 'beta', address: 'host-b', socketPath: '/run/arca/b.sock', viewerTokenPath: '/etc/arca/b.token' },
  ],
  approvalUrl: 'https://approve.example.invalid/arca',
};
// Every name the guard is asked about: each Doctor tool, then tools a coding session has, the reserved transport,
// Arca's real write verb behind arca_propose, and malformed calls.
const PROBES = [
  ...DOCTOR_TOOLS, 'bash', 'str_replace_editor', 'ask_user_question', 'run_code', 'action_propose', 'service_remove',
  'postgres_delete', 'plan_apply', 'web_fetch', 'subagent_spawn', 'ARCA_STATUS', ' arca_status', '', undefined,
];

function validConfig(config = CONFIG) {
  const result = canvas.Config['~standard'].validate(structuredClone(config));
  assert.equal(result.issues, undefined, JSON.stringify(result.issues));
  return result.value;
}

// What the agent-scoped context records: the restriction, the dispatch policies and the prompt sections.
function agentScope() {
  const scope = { restrictions: [], guards: [], preExecute: [], sections: [], selected: [] };
  scope.ctx = {
    tools: {
      restrict(filter) { scope.restrictions.push(filter); return () => {}; },
      guard(guard) { scope.guards.push(guard); return () => {}; },
    },
    on(event, handler) {
      if (event === 'tools/pre-execute') scope.preExecute.push(handler);
      return () => {};
    },
    systemPrompt: { section(section) { scope.sections.push(section); return () => {}; } },
  };
  return scope;
}

// The canvas web chat: its Doctor session opened through createDoctorChat with a DSH stand-in.
async function openWebDoctor(config) {
  const dir = await mkdtemp(join(tmpdir(), 'arca-doctor-web-'));
  const scope = agentScope();
  const registered = [];
  let agentOptions;
  const dsh = {
    tools: {
      register(definition) { registered.push(definition); return () => {}; },
      restrict() { return () => {}; },
      definitions() { return registered; },
    },
    agents: {
      async create(options) {
        agentOptions = options.agentOptions;
        await options.setup(scope.ctx);
        return { agent: { session: { id: options.sessionId } }, dispose: async () => {} };
      },
    },
  };
  const readers = createHostReaders(config);
  const chat = createDoctorChat({
    ...doctorOptions(config, readers),
    resolveDsh: () => dsh,
    store: new ChatStore(join(dir, 'chat.json'), { now: () => NOW }),
    now: () => NOW,
  });
  await chat.ensureAgent();
  await chat.close();
  await rm(dir, { recursive: true, force: true });
  return { scope, tools: registered, model: { provider: agentOptions.provider, model: agentOptions.model } };
}

// A dsh-tui host as the terminal entry sees it inside its agent preset: preset-scoped tool registrations, the agent
// registry's creation edge, and agents that either joined this preset (they see its tools) or did not.
function terminalHost({ titles } = {}) {
  const host = { registered: [], listeners: new Map(), disposers: [], renamed: [] };
  const view = (agent) => (agent.joined ? new Map(host.registered.map((definition) => [definition.name, definition])) : new Map());
  host.ctx = {
    tools: {
      register(definition) { host.registered.push(definition); return () => {}; },
      get(name, agent) { return view(agent).get(name); },
    },
    agents: { list: () => [] },
    on(event, handler) {
      host.listeners.set(event, [...(host.listeners.get(event) ?? []), handler]);
      return () => {};
    },
    effect(effect) { host.disposers.push(effect()); },
    get(name) {
      if (name === 'sessionTitle' && titles) {
        return { get: () => ({ title: 'Untitled' }), rename: (session, title) => host.renamed.push([session.id, title]) };
      }
      return undefined;
    },
    logger: { info() {}, warn() {} },
  };
  host.create = async ({ joined = true, scope = agentScope() } = {}) => {
    const agent = {
      id: `agent-${host.listeners.size}-${Math.random()}`,
      joined,
      ctx: scope.ctx,
      session: { id: 'terminal-session' },
      model: { select(selection) { scope.selected.push(selection); } },
    };
    for (const listener of host.listeners.get('agent/created') ?? []) await listener({ agent, source: 'fresh' });
    return { agent, scope };
  };
  return host;
}

async function openTerminalDoctor(config, options) {
  const host = terminalHost(options);
  const realNow = Date.now;
  Date.now = () => NOW;
  try {
    terminal.apply(host.ctx, config);
    const { scope } = await host.create();
    return { host, scope, tools: host.registered, model: scope.selected.at(-1) };
  } finally {
    Date.now = realNow;
  }
}

// The model-facing part of a tool: what DSH projects onto the wire.
function wire(tools) {
  return JSON.stringify(tools.map(({ name, description, parameters }) => ({ name, description, parameters })));
}

async function decisions(scope) {
  const out = [];
  for (const name of PROBES) {
    const exec = name === undefined ? undefined : { name, arguments: {} };
    const [preExecute] = scope.preExecute;
    const allowed = { kind: 'allow' };
    out.push({
      name,
      preExecute: await preExecute(exec, () => Promise.resolve(allowed)),
      guard: scope.guards.map((guard) => guard(exec)),
    });
  }
  return out;
}

test('the terminal entry is a DSH plugin that takes the canvas config shape unchanged', () => {
  assert.equal(typeof terminal.apply, 'function');
  assert.equal(typeof terminal.name, 'string');
  assert.equal(terminal.Config, canvas.Config, 'one config schema for the canvas and the terminal');
  assert.deepEqual([...terminal.inject].sort(), ['agents', 'tools']);
});

test('the terminal Doctor and the web Doctor carry byte-identical prompt text', async () => {
  const config = validConfig();
  const web = await openWebDoctor(config);
  const tui = await openTerminalDoctor(config);
  assert.ok(web.scope.sections.length >= 2, 'the web Doctor installs its prompt sections');
  assert.equal(JSON.stringify(tui.scope.sections), JSON.stringify(web.scope.sections));
  const doctor = tui.scope.sections.find((section) => section.name === 'arca-doctor');
  assert.match(doctor.text, /You can read live Arca data from these hosts: alpha, beta\./);
  assert.match(doctor.text, /The approval page is at https:\/\/approve\.example\.invalid\/arca\./);
});

test('the terminal Doctor and the web Doctor expose the same tool list, in the same order, with the same schemas', async () => {
  const config = validConfig();
  const web = await openWebDoctor(config);
  const tui = await openTerminalDoctor(config);
  assert.deepEqual(tui.tools.map((tool) => tool.name), [...DOCTOR_TOOLS]);
  assert.equal(wire(tui.tools), wire(web.tools));
  assert.equal(JSON.stringify(tui.scope.restrictions), JSON.stringify(web.scope.restrictions));
  assert.deepEqual([...tui.scope.restrictions.at(-1).allow], [...DOCTOR_TOOLS]);
});

test('the terminal Doctor and the web Doctor make the same guard decision for every call', async () => {
  const config = validConfig();
  const web = await openWebDoctor(config);
  const tui = await openTerminalDoctor(config);
  assert.equal(tui.scope.preExecute.length, 1, 'one pre-execute policy');
  assert.equal(tui.scope.guards.length, 1, 'one monotonic guard');
  assert.deepEqual(await decisions(tui.scope), await decisions(web.scope));
});

test('a tool outside the allow-list is refused in the terminal session exactly as in the web', async () => {
  const config = validConfig();
  const web = await openWebDoctor(config);
  const tui = await openTerminalDoctor(config);
  for (const name of ['bash', 'run_code', 'action_propose', 'service_remove', 'ask_user_question']) {
    const exec = { name, arguments: {} };
    const next = () => assert.fail(`${name} must not reach dispatch`);
    const refusal = await tui.scope.preExecute[0](exec, next);
    assert.equal(refusal.kind, 'deny', `${name} is denied before dispatch`);
    assert.deepEqual(refusal, await web.scope.preExecute[0](exec, next));
    const reason = tui.scope.guards[0](exec);
    assert.match(reason, /The Doctor may only read or propose/);
    assert.equal(reason, web.scope.guards[0](exec));
  }
  for (const name of DOCTOR_TOOLS) assert.equal(tui.scope.guards[0]({ name }), undefined, `${name} is allowed`);
});

test('the terminal Doctor runs on the web Doctor\'s model', async () => {
  const config = validConfig();
  const web = await openWebDoctor(config);
  const tui = await openTerminalDoctor(config);
  assert.deepEqual(web.model, { ...DOCTOR_MODEL });
  assert.deepEqual(tui.model, web.model);
});

test('arca_propose stays the only write-shaped tool of the terminal Doctor', async () => {
  const config = validConfig();
  const tui = await openTerminalDoctor(config);
  const writeShaped = tui.tools.filter((tool) => !READ_TOOLS.includes(tool.name));
  assert.deepEqual(writeShaped.map((tool) => tool.name), [PROPOSE_TOOL]);
  for (const tool of tui.tools) {
    assert.equal(tool.isConcurrencySafe(), tool.name !== PROPOSE_TOOL, `${tool.name} concurrency marks it read-only`);
  }
  // Every read reaches the kernel as that same read verb; the propose tool reaches only action_propose (and the
  // page host's arca_status for its linked machines). No tool can call a write verb.
  const calls = [];
  const client = {
    async call(verb, input) {
      calls.push(verb);
      if (verb === 'action_propose') return { id: 'act-0123456789ab', summary: 'A change', state: 'waiting' };
      if (verb === 'arca_status') return { approval: { linkedHosts: ['beta'] } };
      return {};
    },
  };
  // The terminal registers exactly what the shared factory builds; run the factory's tools against a recording client.
  const options = doctorOptions(config, createHostReaders(config));
  const shared = createDoctorTools({ ...options, clientFor: () => client });
  assert.equal(wire(shared), wire(tui.tools));
  for (const tool of shared) {
    const args = tool.name === PROPOSE_TOOL
      ? { host: 'beta', action: 'service_remove', args: { name: 'app' } }
      : { id: 'abcdefgh', name: 'job' };
    await tool.execute(args, { callId: tool.name });
  }
  assert.deepEqual([...new Set(calls)].sort(), [...READ_TOOLS, 'action_propose'].sort());
});

test('the terminal entry fences only the agents of its own preset, and fails closed when it cannot', async () => {
  const config = validConfig();
  const host = terminalHost();
  terminal.apply(host.ctx, config);
  const outside = await host.create({ joined: false });
  assert.equal(outside.scope.restrictions.length, 0, 'a session of another preset is left alone');
  assert.equal(outside.scope.sections.length, 0);
  const noGuard = agentScope();
  noGuard.ctx.tools.guard = undefined;
  await assert.rejects(host.create({ scope: noGuard }), /restriction is unavailable/);
  const noRestrict = agentScope();
  noRestrict.ctx.tools.restrict = undefined;
  await assert.rejects(host.create({ scope: noRestrict }), /restriction is unavailable/);
});

test('the terminal session is titled Doctor, like the canvas session', async () => {
  const tui = await openTerminalDoctor(validConfig(), { titles: true });
  assert.deepEqual(tui.host.renamed, [['terminal-session', 'Doctor']]);
});

test('the shipped preset patch opens the Doctor preset with the canvas config block, unchanged', async () => {
  const preset = yaml.load(await readFile(new URL('../doctor.patch.yml', import.meta.url), 'utf8'));
  const web = yaml.load(await readFile(new URL('../cordis.patch.yml', import.meta.url), 'utf8'));
  const [row] = preset[0].insert;
  assert.equal(row.name, '@deepseek-ai/dsh-agent-preset');
  assert.equal(row.config.id, DOCTOR_PRESET);
  assert.equal(row.config.plugins.length, 1, 'the preset composes the Doctor and nothing else');
  const [child] = row.config.plugins;
  assert.equal(child.name, 'arca-canvas-dsh/doctor');
  assert.deepEqual(child.config, web[0].insert[0].config, 'one config block works for both');
  validConfig(child.config);
});

test('the package ships the terminal entry and leaves the browser build alone', async () => {
  const manifest = JSON.parse(await readFile(new URL('../package.json', import.meta.url), 'utf8'));
  for (const file of ['doctor.js', 'doctor.patch.yml', 'src/doctor-session.js']) {
    assert.ok(manifest.files.includes(file), `${file} is published`);
  }
  assert.equal(manifest.exports['./doctor'], './doctor.js');
  assert.deepEqual(manifest.dsh.bundle, { patch: './cordis.patch.yml' }, 'web profiles do not gain the preset');
  const { buildBrowser } = await import('../build.mjs');
  const dir = await mkdtemp(join(tmpdir(), 'arca-build-'));
  try {
    await buildBrowser(dir);
    for (const file of ['client.js', 'page.js']) {
      const built = await readFile(join(dir, file), 'utf8');
      const shipped = await readFile(new URL(`../${file}`, import.meta.url), 'utf8');
      assert.equal(built === shipped, true, `${file} is unchanged by the build`);
    }
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

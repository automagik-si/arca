// The Doctor in a terminal (dsh-tui) must be the canvas Doctor: the same prompt, the same tools and the same guard.
// Both entries build their sessions from src/doctor-session.js, so the side-by-side comparisons below hold by
// construction: they are fork detectors, and fail only if one entry stops using the shared module. What the fences
// actually do is pinned with literal expectations here, and on a real DSH in doctor-preset-real-dsh.test.js.
import assert from 'node:assert/strict';
import { chmod, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import yaml from 'js-yaml';
import * as canvas from '../index.js';
import * as terminal from '../doctor.js';
import { findDoctorPreset, main as launch } from '../doctor-launch.js';
import { ChatStore, createDoctorChat } from '../src/chat.js';
import {
  DOCTOR_LIMITS, DOCTOR_MODEL, DOCTOR_PRESET, DOCTOR_TOOLS, PROPOSE_TOOL, createDoctorTools, createHostReaders,
  doctorOptions,
} from '../src/doctor-session.js';

const NOW = Date.parse('2026-11-15T12:00:00Z');
// The read verbs, written out: a verb added to the shared list must be added here on purpose.
const READS = [
  'arca_status', 'watch_report', 'postgres_list', 'node_metrics', 'alerts_list', 'plan_list', 'journal_list',
  'service_list', 'storage_list', 'snapshot_list', 'tenant_list', 'tenant_status', 'tenant_usage', 'function_list',
  'function_runs',
];
const DENIAL = 'The Doctor may only read or propose: that tool is not available.';
const STATE_DIR = await mkdtemp(join(tmpdir(), 'arca-doctor-terminal-'));
test.after(() => rm(STATE_DIR, { recursive: true, force: true }));
const CONFIG = {
  hosts: [
    { name: 'alpha', address: 'host-a', socketPath: '/run/arca/a.sock', viewerTokenPath: '/etc/arca/a.token' },
    { name: 'beta', address: 'host-b', socketPath: '/run/arca/b.sock', viewerTokenPath: '/etc/arca/b.token' },
  ],
  approvalUrl: 'https://approve.example.invalid/arca',
  chatStatePath: join(STATE_DIR, 'chat.json'),
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
  const scope = { restrictions: [], guards: [], preExecute: [], sections: [], selected: [], listeners: new Map() };
  scope.ctx = {
    tools: {
      restrict(filter) { scope.restrictions.push(filter); return () => {}; },
      guard(guard) { scope.guards.push(guard); return () => {}; },
    },
    on(event, handler) {
      if (event === 'tools/pre-execute') scope.preExecute.push(handler);
      scope.listeners.set(event, [...(scope.listeners.get(event) ?? []), handler]);
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

// A dsh-tui host as the terminal entry sees it inside its agent preset: preset-scoped tool registrations with their
// step conditions, the agent registry (its creation edge and its live list), tools/change, and agents that either
// joined this preset (they see its tools) or did not.
function terminalHost({ titles } = {}) {
  const host = { registered: [], when: new Map(), listeners: new Map(), live: [], renamed: [] };
  const view = (agent) => (agent.joined ? new Map(host.registered.map((definition) => [definition.name, definition])) : new Map());
  host.ctx = {
    tools: {
      register(definition, options = {}) {
        host.registered.push(definition);
        host.when.set(definition.name, options.when);
        return () => {};
      },
      get(name, agent) { return view(agent).get(name); },
    },
    agents: { list: () => [...host.live] },
    on(event, handler) {
      host.listeners.set(event, [...(host.listeners.get(event) ?? []), handler]);
      return () => {};
    },
    get(name) {
      if (name === 'sessionTitle' && titles) {
        return { get: () => ({ title: 'Untitled' }), rename: (session, title) => host.renamed.push([session.id, title]) };
      }
      return undefined;
    },
    logger: { info() {}, warn() {} },
  };
  host.emit = (event, payload) => {
    for (const listener of host.listeners.get(event) ?? []) listener(payload);
  };
  host.create = async ({ joined = true, scope = agentScope() } = {}) => {
    const agent = {
      id: `agent-${host.live.length}`,
      joined,
      ctx: scope.ctx,
      session: { id: `session-${host.live.length}` },
      model: { select(selection) { scope.selected.push(selection); } },
      cancels: [],
      cancel(reason) { this.cancels.push(reason); },
    };
    host.live.push(agent);
    for (const listener of host.listeners.get('agent/created') ?? []) await listener({ agent, source: 'fresh' });
    return { agent, scope };
  };
  // /preset arca-doctor on a blank session: the registry rebinds the live agent and emits only tools/change.
  host.switchIn = (agent) => {
    agent.joined = true;
    host.emit('tools/change');
  };
  // The step admission DSH computes before a model request: a registration's condition decides.
  host.stepTools = (agent) => host.registered
    .filter((definition) => view(agent).has(definition.name) && (host.when.get(definition.name)?.({ agent }) ?? true))
    .map((definition) => definition.name);
  return host;
}

// A session event as the agent loop appends it, delivered to the agent's scoped listeners.
function sessionEvent(agent, scope, type, data = {}) {
  for (const listener of scope.listeners.get('session/event') ?? []) listener(agent.session, { type, data });
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 20));

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

test('fork detector: the terminal and the web Doctor carry byte-identical prompt text', async () => {
  const config = validConfig();
  const web = await openWebDoctor(config);
  const tui = await openTerminalDoctor(config);
  assert.ok(web.scope.sections.length >= 2, 'the web Doctor installs its prompt sections');
  assert.equal(JSON.stringify(tui.scope.sections), JSON.stringify(web.scope.sections));
  const doctor = tui.scope.sections.find((section) => section.name === 'arca-doctor');
  assert.match(doctor.text, /You can read live Arca data from these hosts: alpha, beta\./);
  assert.match(doctor.text, /The approval page is at https:\/\/approve\.example\.invalid\/arca\./);
});

test('fork detector: the terminal and the web Doctor expose the same tool list, order and schemas', async () => {
  const config = validConfig();
  const web = await openWebDoctor(config);
  const tui = await openTerminalDoctor(config);
  assert.deepEqual(tui.tools.map((tool) => tool.name), [...DOCTOR_TOOLS]);
  assert.equal(wire(tui.tools), wire(web.tools));
  assert.equal(JSON.stringify(tui.scope.restrictions), JSON.stringify(web.scope.restrictions));
  assert.deepEqual([...tui.scope.restrictions.at(-1).allow], [...DOCTOR_TOOLS]);
});

test('fork detector: the terminal and the web Doctor make the same guard decision for every call', async () => {
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
  for (const name of [...READS, 'arca_propose']) assert.equal(tui.scope.guards[0]({ name }), undefined, `${name} is allowed`);
  for (const name of PROBES.filter((probe) => ![...READS, 'arca_propose'].includes(probe))) {
    assert.equal(tui.scope.guards[0](name === undefined ? undefined : { name }), DENIAL, `${String(name)} is refused`);
  }
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
  assert.deepEqual(tui.tools.map((tool) => tool.name), [...READS, 'arca_propose']);
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
  assert.deepEqual([...new Set(calls)].sort(), [...READS, 'action_propose'].sort());
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
  assert.deepEqual(tui.host.renamed, [['session-0', 'Doctor']]);
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
  for (const file of ['doctor.js', 'doctor-launch.js', 'doctor.patch.yml', 'src/doctor-session.js']) {
    assert.ok(manifest.files.includes(file), `${file} is published`);
  }
  assert.equal(manifest.exports['./doctor'], './doctor.js');
  assert.deepEqual(manifest.bin, { 'arca-doctor': './doctor-launch.js' }, 'the launcher is the one way a bot starts it');
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

test('a blank session switched to the preset is fenced like a created one, with no creation edge', async () => {
  const host = terminalHost();
  terminal.apply(host.ctx, validConfig());
  const { agent, scope } = await host.create({ joined: false });
  assert.equal(scope.restrictions.length, 0, 'a session of another preset is left alone');
  host.switchIn(agent);
  assert.deepEqual([...scope.restrictions.at(-1).allow], [...READS, 'arca_propose']);
  assert.equal(scope.guards.length, 1);
  assert.equal(scope.guards[0]({ name: 'bash' }), DENIAL);
  assert.equal((await scope.preExecute[0]({ name: 'bash' }, () => assert.fail('bash must not dispatch'))).reason, DENIAL);
  assert.deepEqual(scope.sections.map((section) => section.name), ['deployment:persona-prefix', 'arca-doctor']);
  assert.deepEqual(scope.selected, [{ ...DOCTOR_MODEL }]);
  assert.deepEqual(host.stepTools(agent), [...READS, 'arca_propose']);
  host.emit('tools/change');
  assert.equal(scope.guards.length, 1, 'a later tools/change installs nothing twice');
});

test('a switched session the fences cannot be installed on refuses every call and gets no Doctor tool', async () => {
  const host = terminalHost();
  terminal.apply(host.ctx, validConfig());
  const scope = agentScope();
  scope.ctx.tools.restrict = undefined;
  const { agent } = await host.create({ joined: false, scope });
  host.switchIn(agent);
  assert.equal(scope.guards.length, 1, 'a guard that refuses everything');
  assert.equal(scope.guards[0]({ name: 'arca_status' }), terminal.SETUP_FAILED);
  assert.equal(scope.guards[0]({ name: 'bash' }), terminal.SETUP_FAILED);
  assert.deepEqual(host.stepTools(agent), [], 'the Doctor\'s tools stay out of its steps');
});

test('the Doctor\'s tools reach a model step only for an agent the terminal entry fenced', async () => {
  const host = terminalHost();
  terminal.apply(host.ctx, validConfig());
  const { agent } = await host.create();
  assert.deepEqual(host.stepTools(agent), [...READS, 'arca_propose']);
  const stranger = { id: 'x', joined: true, ctx: agentScope().ctx, session: { id: 'x' } };
  assert.deepEqual(host.stepTools(stranger), [], 'joined but never fenced: no Doctor tool');
});

test('a terminal Doctor request carries the canvas output cap and keeps its route', async () => {
  const host = terminalHost();
  terminal.apply(host.ctx, validConfig());
  const { scope } = await host.create();
  const [request] = scope.listeners.get('agent/request');
  const proposed = await request({ turn: 1, step: 1 }, () => Promise.resolve({ ...DOCTOR_MODEL, maxTokens: 256_000 }));
  assert.deepEqual(proposed, { ...DOCTOR_MODEL, maxTokens: 64_000 });
  assert.equal(DOCTOR_LIMITS.outputTokensPerTurn, 64_000);
});

test('a terminal turn stops at the canvas\'s 40-tool-call cap and starts over on the next turn', async () => {
  const host = terminalHost();
  terminal.apply(host.ctx, validConfig());
  const { agent, scope } = await host.create();
  const tool = host.registered.find((definition) => definition.name === 'arca_status');
  const call = (index) => tool.execute({}, { callId: `c${index}`, agent }).then(() => 'ran', (error) => error.message);
  assert.equal(await call(0), 'No active Doctor turn.', 'no call is counted outside a turn');
  sessionEvent(agent, scope, 'turn/start', { turn: 1 });
  for (let index = 1; index <= 40; index += 1) {
    assert.doesNotMatch(await call(index), /tool-call limit/, `call ${index} is within the cap`);
  }
  assert.equal(await call(41), 'This answer stopped at the 40-tool-call limit for one turn.');
  assert.deepEqual(agent.cancels, ['tool-calls']);
  assert.equal(await call(42), 'This answer stopped at the 40-tool-call limit for one turn.');
  sessionEvent(agent, scope, 'turn/end', { turn: 1 });
  sessionEvent(agent, scope, 'turn/start', { turn: 2 });
  assert.doesNotMatch(await call(43), /tool-call limit/, 'a new turn has a new count');
});

test('a terminal turn stops past the canvas\'s per-turn output tokens', async () => {
  const host = terminalHost();
  terminal.apply(host.ctx, validConfig({ ...CONFIG, chatStatePath: join(await mkdtemp(join(STATE_DIR, 'out-')), 'c.json') }));
  const { agent, scope } = await host.create();
  sessionEvent(agent, scope, 'turn/start', { turn: 1 });
  sessionEvent(agent, scope, 'assistant/message', { turn: 1, usage: { inputTokens: 10, outputTokens: 40_000 } });
  assert.deepEqual(agent.cancels, []);
  sessionEvent(agent, scope, 'assistant/message', { turn: 1, usage: { inputTokens: 10, outputTokens: 30_000 } });
  assert.deepEqual(agent.cancels, ['output-tokens']);
  await settle();
});

test('the terminal honours the daily token budget: the turn that spends it stops, the next step is refused', async () => {
  const dir = await mkdtemp(join(STATE_DIR, 'budget-'));
  const config = validConfig({ ...CONFIG, chatStatePath: join(dir, 'chat.json'), dailyTokenBudget: 100 });
  assert.equal(terminal.terminalStatePath(config), join(dir, 'arca-doctor-terminal.json'), 'beside the canvas file, not in it');
  const host = terminalHost();
  terminal.apply(host.ctx, config);
  const { agent, scope } = await host.create();
  const [preStep] = scope.listeners.get('agent/pre-step');
  const enter = { kind: 'enter', messages: [] };
  assert.deepEqual(await preStep({}, () => Promise.resolve(enter)), enter, 'a step under the budget enters');
  sessionEvent(agent, scope, 'turn/start', { turn: 1 });
  sessionEvent(agent, scope, 'assistant/message', { turn: 1, usage: { totalTokens: 150 } });
  for (let wait = 0; wait < 50 && !agent.cancels.length; wait += 1) await settle();
  assert.deepEqual(agent.cancels, ['budget']);
  assert.deepEqual(await preStep({}, () => assert.fail('a spent budget must not reach the model')), { kind: 'reject' });
  const stored = JSON.parse(await readFile(join(dir, 'arca-doctor-terminal.json'), 'utf8'));
  assert.equal(stored.dailyTokens, 150);
});

test('chatEnabled: false keeps the terminal Doctor from starting', () => {
  const host = terminalHost();
  assert.throws(() => terminal.apply(host.ctx, validConfig({ ...CONFIG, chatEnabled: false })), /switched off in this config/);
  assert.equal(host.registered.length, 0);
});

// What `dsh --dump-config` prints for a profile with the shipped preset, and variants that must not count.
const PRESET_ROW = [
  '- id: arca-doctor-preset',
  "  name: '@deepseek-ai/dsh-agent-preset'",
  '  config:',
  '    id: arca-doctor',
  '    name: Arca Doctor',
  '    plugins:',
  '      - name: arca-canvas-dsh/doctor',
  '        config:',
  '          socketPath: /run/arca/arca.sock',
].join('\n');
const OTHER_ROWS = "- id: agent\n  name: '@deepseek-ai/dsh-agent'\n# == a bundle\n- id: tool-bash\n  name: '@deepseek-ai/dsh-tool-bash'\n";

test('the launcher finds the Doctor preset only when it is in the profile and switched on', () => {
  assert.equal(findDoctorPreset(`${OTHER_ROWS}${PRESET_ROW}\n${OTHER_ROWS}`), true);
  assert.equal(findDoctorPreset(OTHER_ROWS), false, 'no preset row');
  assert.equal(findDoctorPreset(PRESET_ROW.replace('id: arca-doctor\n', 'id: standard\n')), false, 'another preset id');
  assert.equal(findDoctorPreset(PRESET_ROW.replace('arca-canvas-dsh/doctor', 'other/plugin')), false, 'no Doctor plugin');
  assert.equal(findDoctorPreset(PRESET_ROW.replace('  config:\n', '  disabled: true\n  config:\n')), false, 'disabled');
  assert.equal(findDoctorPreset(PRESET_ROW.replace('  config:\n', '  disabled: !!js >-\n    false\n  config:\n')), false,
    'a disabled expression cannot be evaluated here, so it counts as disabled');
  assert.equal(findDoctorPreset(PRESET_ROW.replace('  config:\n', '  disabled: false\n  config:\n')), true);
});

test('the launcher refuses to start without the preset and starts dsh-tui pinned to the preset', async (t) => {
  const dir = await mkdtemp(join(STATE_DIR, 'launch-'));
  const dump = join(dir, 'dump.yml');
  const out = join(dir, 'run.json');
  const fake = join(dir, 'dsh');
  await writeFile(fake, `#!${process.execPath}
const fs = require('node:fs');
const args = process.argv.slice(2);
if (args.includes('--dump-config')) { process.stdout.write(fs.readFileSync(${JSON.stringify(dump)}, 'utf8')); process.exit(0); }
fs.writeFileSync(${JSON.stringify(out)}, JSON.stringify({ args, preset: process.env.DSH_TUI_PRESET }));
process.exit(7);
`);
  await chmod(fake, 0o755);
  const env = { ...process.env, ARCA_DOCTOR_DSH: fake };
  const said = t.mock.method(process.stderr, 'write', () => true);
  await writeFile(dump, OTHER_ROWS);
  assert.equal(launch(['--profile', 'bot'], env), 2, 'no preset: refused');
  assert.match(said.mock.calls.map((c) => c.arguments[0]).join(''), /has no arca-doctor preset switched on, so the Doctor was not started/);
  assert.equal(launch([], env), 2, 'no profile: refused');
  assert.equal(launch(['--profile', 'bot'], { ...env, ARCA_DOCTOR_DSH: join(dir, 'missing') }), 1, 'no dsh: refused');
  await writeFile(dump, `${OTHER_ROWS}${PRESET_ROW}\n`);
  assert.equal(launch(['--profile', 'bot', '--patch', '/p.yml', '--check'], env), 0);
  await assert.rejects(readFile(out), 'a check starts nothing');
  assert.equal(launch(['--profile', 'bot', '--patch', '/p.yml', '--resume', 'abc'], env), 7, 'dsh-tui\'s exit code');
  assert.deepEqual(JSON.parse(await readFile(out, 'utf8')), {
    args: ['--profile', 'bot', '--patch', '/p.yml', '--resume', 'abc'], preset: 'arca-doctor',
  });
});

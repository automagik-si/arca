// Runs inside a real DSH host (test/doctor-preset-real-dsh.test.js). It opens Doctor sessions the two ways a dsh-tui
// session reaches the arca-doctor preset, through the host's own agent factory and preset registry, and writes what
// each agent can see and call. No model is asked: creating an agent and selecting its preset send no request.
import { randomUUID } from 'node:crypto';
import { writeFileSync } from 'node:fs';

export const name = 'arca-doctor-preset-probe';
export const inject = ['agents', 'agentPresets', 'tools'];

const PROBES = ['bash', 'str_replace_editor', 'run_code', 'action_propose', 'arca_status', 'arca_propose'];
const ROUTE = { provider: 'deepseek-official', model: 'deepseek-flash' };

export function apply(ctx, config) {
  // What a model step admits (conditional registrations resolved), as the agent loop asks before each request.
  const stepTools = async (agent) => {
    const set = await ctx.tools.resolve({ agent, modelSelection: agent.model.current(), signal: new AbortController().signal });
    try {
      return [...set.view(agent).keys()].sort();
    } finally {
      set.dispose();
    }
  };
  const seen = async (agent, how) => ({
    how,
    step: await stepTools(agent),
    preset: ctx.agentPresets.composedPreset(agent.ctx) ?? null,
    tools: ctx.tools.definitions(agent).map((tool) => tool.name).sort(),
    guard: Object.fromEntries(PROBES.map((tool) => [tool, ctx.tools.checkGuards({ agent, name: tool }) ?? null])),
    model: agent.model?.current?.() ? { provider: agent.model.current().provider, model: agent.model.current().model } : null,
  });
  const open = (preset) => ctx.agents.create({
    sessionId: randomUUID(),
    agentOptions: { provider: 'deepseek-official', model: 'deepseek-v4-pro' },
    // As dsh-tui does: the setup mounts the preset and returns nothing.
    setup: async (agentCtx) => { await ctx.agentPresets.mount(agentCtx, preset); },
  });

  const run = async () => {
    const out = { route: ROUTE, agents: [] };
    try {
      // A session created on the preset: DSH_TUI_PRESET=arca-doctor, or a resume of a Doctor session.
      const created = await open('arca-doctor');
      out.agents.push(await seen(created.agent, 'created'));
      // A blank session of the default preset switched with /preset arca-doctor (the registry's select, which
      // rebinds the live agent and emits only tools/change).
      const blank = await open('standard');
      out.agents.push(await seen(blank.agent, 'blank-before-switch'));
      await ctx.agentPresets.select(blank.agent, 'arca-doctor');
      out.agents.push(await seen(blank.agent, 'switched'));
    } catch (error) {
      out.error = String(error?.stack ?? error);
    }
    writeFileSync(config.out, `${JSON.stringify(out)}\n`);
  };
  // Start once the host's own composition has settled.
  const timer = setTimeout(() => { void run(); }, 1500);
  ctx.effect(() => () => clearTimeout(timer));
}

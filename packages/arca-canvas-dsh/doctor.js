// The Arca Doctor in a terminal: the canvas Doctor's session as a DSH agent preset, for dsh-tui and the Herdr bots
// that run it. doctor.patch.yml declares the `arca-doctor` preset with this plugin as its only child, and
// doctor-launch.js opens a dsh-tui session on it.
//
// Nothing here defines the Doctor: the prompt, the tools, the allow-list, the restriction, pre-execute policy and
// guard, the model, the limits and the hosts config all come from src/doctor-session.js, the module the canvas chat
// uses. This file only attaches them to each terminal agent and counts the canvas's limits against it.
import { homedir } from 'node:os';
import { dirname, join } from 'node:path';
import { ChatStore } from './src/chat.js';
import {
  Config, DOCTOR_LIMITS, DOCTOR_MODEL, DOCTOR_SESSION_NAME, PROPOSE_TOOL, capMessage, createDoctorTools,
  createHostReaders, doctorOptions, installDoctorSession,
} from './src/doctor-session.js';

export const name = 'arca-doctor';
export const inject = ['tools', 'agents'];
export { Config };

export const SETUP_FAILED = 'The Doctor could not be set up for this session, so it may not call any tool.';
export const BUDGET_REACHED = 'Daily budget reached, try tomorrow.';

// The terminal's daily counter. It sits beside the canvas's chat state (chatStatePath) when that is set, never in it:
// the canvas file also holds the canvas session and transcript.
export function terminalStatePath(config, env = process.env) {
  const configured = String(config.chatStatePath ?? '').trim();
  if (configured) return join(dirname(configured), 'arca-doctor-terminal.json');
  const home = String(env.DSH_HOME ?? '').trim() || join(homedir(), '.dsh');
  return join(home, 'storages', 'arca-doctor-terminal.json');
}

export function apply(ctx, config) {
  if (config.chatEnabled === false) {
    throw new Error('The Doctor is switched off in this config (chatEnabled: false), so the terminal Doctor does not start.');
  }
  const log = (message) => ctx.logger?.warn?.(`Arca Doctor: ${message}`);
  const budget = config.dailyTokenBudget ?? DOCTOR_LIMITS.dailyTokenBudget;
  const daily = new ChatStore(terminalStatePath(config));
  const options = doctorOptions(config, createHostReaders(config));
  const fenced = new WeakSet();
  const installing = new WeakSet();
  const turns = new WeakMap();

  // A per-turn counter, as the canvas keeps one per answer: tool calls, output tokens and a deadline.
  function stop(agent, reason) {
    const turn = turns.get(agent);
    if (!turn || turn.reason) return;
    turn.reason = reason;
    if (reason === 'tool-calls' || reason === 'output-tokens') {
      const limit = reason === 'tool-calls' ? DOCTOR_LIMITS.toolCallsPerTurn : DOCTOR_LIMITS.outputTokensPerTurn;
      log(capMessage(reason, limit));
    } else if (reason === 'budget') {
      log(BUDGET_REACHED);
    } else {
      log('The Doctor took too long and the answer was stopped.');
    }
    try { agent.cancel?.(reason); } catch { /* already quiescent */ }
  }

  function onCall({ agent }) {
    const turn = agent ? turns.get(agent) : undefined;
    if (!turn) throw new Error('No active Doctor turn.');
    if (turn.reason) throw new Error(turn.reason === 'tool-calls'
      ? capMessage('tool-calls', DOCTOR_LIMITS.toolCallsPerTurn)
      : 'The Doctor turn was stopped.');
    if (turn.toolCalls >= DOCTOR_LIMITS.toolCallsPerTurn) {
      stop(agent, 'tool-calls');
      throw new Error(capMessage('tool-calls', DOCTOR_LIMITS.toolCallsPerTurn));
    }
    turn.toolCalls += 1;
  }

  // Inside the preset these registrations belong to the preset's scope. They are admitted to a model step only for
  // an agent this plugin has fenced, so an agent that joined the preset some other way never gets them.
  const tools = createDoctorTools({ ...options, onCall });
  for (const tool of tools) ctx.tools.register(tool, { when: (context) => fenced.has(context?.agent) });
  const propose = tools.find((tool) => tool.name === PROPOSE_TOOL);

  function attachLimits(agent) {
    const agentCtx = agent.ctx;
    // Each model request carries the canvas's output cap, as the canvas sets it at creation.
    agentCtx.on('agent/request', (input, next) => next().then((request) => ({
      ...request, maxTokens: DOCTOR_LIMITS.outputTokensPerTurn,
    })));
    // A spent daily budget refuses the next step before the model is asked.
    agentCtx.on('agent/pre-step', async (input, next) => {
      const state = await daily.read();
      if (state.budgetExhausted || state.dailyTokens >= budget) {
        log(BUDGET_REACHED);
        return { kind: 'reject' };
      }
      return next();
    });
    agentCtx.on('session/event', (session, event) => {
      if (!event || session?.id !== agent.session?.id) return;
      if (event.type === 'turn/start') {
        clearTimeout(turns.get(agent)?.timer);
        const turn = { toolCalls: 0, outputTokens: 0, reason: null, timer: null };
        turn.timer = setTimeout(() => stop(agent, 'timeout'), DOCTOR_LIMITS.turnMs);
        turn.timer.unref?.();
        turns.set(agent, turn);
      } else if (event.type === 'turn/end') {
        clearTimeout(turns.get(agent)?.timer);
      } else if (event.type === 'assistant/message' && event.data?.usage) {
        const turn = turns.get(agent);
        const usage = event.data.usage;
        const output = Math.max(0, Number(usage.outputTokens) || 0);
        const total = Number(usage.totalTokens) > 0
          ? Number(usage.totalTokens)
          : (Number(usage.inputTokens) || 0) + output;
        void daily.patch((state) => ({ ...state, dailyTokens: state.dailyTokens + total })).then((state) => {
          if (state.dailyTokens >= budget) stop(agent, 'budget');
        }, () => {
          // A count that cannot be written spends the rest of the day, as on the canvas.
          daily.exhaust();
          stop(agent, 'budget');
        });
        if (turn) {
          turn.outputTokens += output;
          if (turn.outputTokens > DOCTOR_LIMITS.outputTokensPerTurn) stop(agent, 'output-tokens');
        }
      }
    });
  }

  // An agent that can see this plugin's tools is a Doctor and gets the Doctor's fences, whatever path brought it into
  // the preset. On the creation edge a failure throws and DSH rolls the agent back unpublished.
  function install(agent) {
    if (!agent?.ctx || fenced.has(agent) || installing.has(agent)) return;
    if (ctx.tools.get(PROPOSE_TOOL, agent) !== propose) return;
    installing.add(agent);
    try {
      installDoctorSession(agent.ctx, { ...options, now: Date.now() });
      attachLimits(agent);
      fenced.add(agent);
    } finally {
      installing.delete(agent);
    }
    // The canvas opens its session on this model; a terminal session records it as the session's selection.
    agent.model?.select?.({ ...DOCTOR_MODEL });
    const titles = ctx.get('sessionTitle');
    if (titles?.rename) {
      try {
        if (titles.get?.(agent.session)?.title !== DOCTOR_SESSION_NAME) titles.rename(agent.session, DOCTOR_SESSION_NAME);
      } catch (error) {
        log(`session title not set (${error?.code ?? error?.name ?? 'error'})`);
      }
    }
  }

  // A live agent can join the preset without a creation edge: a blank session switched with /preset is rebound to
  // the preset and DSH emits only tools/change. Nothing can roll that agent back, so a failure there closes it: a
  // guard that refuses every call, and the Doctor's tools stay out of its steps.
  function installLive(agent) {
    try {
      install(agent);
    } catch (error) {
      log(`${SETUP_FAILED} (${error?.message ?? error})`);
      try { agent.ctx.tools.guard(() => SETUP_FAILED); } catch { /* the tools never reach it: they need the fences */ }
    }
  }

  for (const agent of ctx.agents.list?.() ?? []) installLive(agent);
  ctx.on('agent/created', ({ agent }) => install(agent));
  ctx.on('tools/change', () => {
    for (const agent of ctx.agents.list?.() ?? []) installLive(agent);
  });
}

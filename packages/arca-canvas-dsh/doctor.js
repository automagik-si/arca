// The Arca Doctor in a terminal: the canvas Doctor's session as a DSH agent preset, for dsh-tui and the Herdr bots
// that run it. doctor.patch.yml declares the `arca-doctor` preset with this plugin as its only child, and
// `DSH_TUI_PRESET=arca-doctor` opens a dsh-tui session on it.
//
// Nothing here defines the Doctor: the prompt, the tools, the allow-list, the restriction, pre-execute policy and
// guard, the model and the hosts config all come from src/doctor-session.js, the module the canvas chat uses.
import {
  Config, DOCTOR_MODEL, DOCTOR_SESSION_NAME, PROPOSE_TOOL, createDoctorTools, createHostReaders, doctorOptions,
  installDoctorSession,
} from './src/doctor-session.js';

export const name = 'arca-doctor';
export const inject = ['tools', 'agents'];
export { Config };

export function apply(ctx, config) {
  const options = doctorOptions(config, createHostReaders(config));
  // Inside the preset these registrations belong to the preset's scope: only the agents of this preset see them.
  const tools = createDoctorTools(options);
  for (const tool of tools) ctx.tools.register(tool);
  const propose = tools.find((tool) => tool.name === PROPOSE_TOOL);
  const installed = new WeakSet();

  // An agent that can see this plugin's tools is a Doctor and gets the Doctor's fences, whatever preset id it came
  // from. If they cannot be installed, the creation edge throws and DSH rolls the agent back unpublished.
  function install(agent) {
    if (!agent?.ctx || installed.has(agent) || ctx.tools.get(PROPOSE_TOOL, agent) !== propose) return;
    installDoctorSession(agent.ctx, { ...options, now: Date.now() });
    installed.add(agent);
    // The canvas opens its session on this model; a terminal session records it as the session's selection.
    agent.model?.select?.({ ...DOCTOR_MODEL });
    const titles = ctx.get('sessionTitle');
    if (titles?.rename) {
      try {
        if (titles.get?.(agent.session)?.title !== DOCTOR_SESSION_NAME) titles.rename(agent.session, DOCTOR_SESSION_NAME);
      } catch (error) {
        ctx.logger?.warn?.(`Arca Doctor: session title not set (${error?.code ?? error?.name ?? 'error'})`);
      }
    }
  }

  for (const agent of ctx.agents.list?.() ?? []) install(agent);
  ctx.on('agent/created', ({ agent }) => install(agent));
}

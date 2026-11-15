#!/usr/bin/env node
// Start the terminal Doctor, or refuse to. dsh-tui opens a session on DSH_TUI_PRESET, and a preset id the profile
// does not have is not an error there: the session starts as a full coding agent instead. A bot left alone must not
// get that, so this reads the profile's composed tree first and starts dsh-tui on arca-doctor only when the Doctor
// preset is in it and switched on.
//
//   arca-doctor --profile <profile> [--patch <file>]... [--check] [dsh-tui arguments...]
//
// --patch files are part of the composition, so they are read for the check and passed to dsh-tui. --check reads and
// exits. The dsh program is ARCA_DOCTOR_DSH, else `dsh` on PATH.
import { spawnSync } from 'node:child_process';
import { realpathSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

export const PRESET_ID = 'arca-doctor';
const PRESET_PACKAGE = '@deepseek-ai/dsh-agent-preset';
const DOCTOR_PLUGIN = 'arca-canvas-dsh/doctor';

const value = (text) => text.trim().replace(/^(['"])(.*)\1$/, '$2');

// The composed tree as `dsh --dump-config` prints it: one top-level row per `- ` line, two-space keys inside.
// A row counts only when it is the preset package, its config id is arca-doctor, it composes the Doctor plugin, and
// it is not disabled (a disabled key that is anything but a plain false counts as disabled: it cannot be evaluated
// here).
export function findDoctorPreset(dump) {
  const rows = `\n${dump}`.split(/\n(?=- )/).slice(1);
  for (const row of rows) {
    const lines = row.split('\n');
    const after = (line) => value(line.slice(line.indexOf(':') + 1));
    if (!lines.some((line) => /^(- | {2})name:/.test(line) && after(line) === PRESET_PACKAGE)) continue;
    if (!lines.some((line) => /^ {4}id:/.test(line) && after(line) === PRESET_ID)) continue;
    if (!lines.some((line) => /^\s+- name:/.test(line) && after(line) === DOCTOR_PLUGIN)) continue;
    if (lines.some((line) => /^(- | {2})disabled:/.test(line) && after(line) !== 'false')) continue;
    return true;
  }
  return false;
}

export function parseArgs(argv) {
  const host = [];
  const app = [];
  let profile = '';
  let check = false;
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === '--profile' || arg === '--patch') {
      const next = argv[index + 1];
      if (next === undefined || next.startsWith('--')) return { error: `${arg} needs a value.` };
      if (arg === '--profile') profile = next;
      else host.push(arg, next);
      index += 1;
    } else if (arg === '--check') {
      check = true;
    } else {
      app.push(arg);
    }
  }
  if (!profile) return { error: 'Name the dsh-tui profile with --profile <profile>.' };
  return { profile, host, app, check };
}

export function main(argv = process.argv.slice(2), env = process.env) {
  const say = (line) => process.stderr.write(`arca-doctor: ${line}\n`);
  const args = parseArgs(argv);
  if (args.error) {
    say(args.error);
    return 2;
  }
  const dsh = env.ARCA_DOCTOR_DSH || 'dsh';
  const dump = spawnSync(dsh, ['--profile', args.profile, ...args.host, '--dump-config'], {
    env, encoding: 'utf8', maxBuffer: 64 << 20,
  });
  if (dump.error || dump.status !== 0) {
    say(`could not read profile "${args.profile}" with ${dsh}, so the Doctor was not started.`);
    return 1;
  }
  if (!findDoctorPreset(dump.stdout)) {
    say(`profile "${args.profile}" has no ${PRESET_ID} preset switched on, so the Doctor was not started.`);
    say('Add doctor.patch.yml to the profile\'s cordis.patch.yml (see the arca-canvas-dsh README).');
    return 2;
  }
  if (args.check) {
    say(`profile "${args.profile}" has the ${PRESET_ID} preset.`);
    return 0;
  }
  const run = spawnSync(dsh, ['--profile', args.profile, ...args.host, ...args.app], {
    env: { ...env, DSH_TUI_PRESET: PRESET_ID }, stdio: 'inherit',
  });
  if (run.error) {
    say(`could not start ${dsh}.`);
    return 1;
  }
  return run.status ?? 1;
}

const invoked = process.argv[1] ? realpathSync(process.argv[1]) : '';
if (invoked && invoked === realpathSync(fileURLToPath(import.meta.url))) process.exitCode = main();

#!/bin/bash
# arca-job: runs one GitHub Actions runner, once, in a job machine Arca made for it.
#
# Arca writes this file into the machine and starts it as root through the Incus agent, after it wrote the runner's
# just-in-time configuration to /run/arca-job-jit (memory, mode 0600). It:
# 1. reads the configuration and deletes the file;
# 2. starts the runner as the runner user in a login session, so /etc/environment and the limits apply as on GitHub,
#    with the configuration in the runner's environment only (never on a command line, never on disk again);
# 3. when the runner exits (a just-in-time runner takes exactly one job), packs its output and its _diag folder into
#    /var/log/arca-job/logs.tar.gz and writes the exit code to /var/log/arca-job/exit, which Arca reads back before it
#    destroys the machine.
#
# ARCA_RUNNER_DIR names the runner's folder (default /home/runner/actions-runner), ARCA_RUNNER_USER its user.
set -u

RUNNER_DIR=${ARCA_RUNNER_DIR:-/home/runner/actions-runner}
RUNNER_USER=${ARCA_RUNNER_USER:-runner}
JIT_FILE=/run/arca-job-jit
OUT=/var/log/arca-job

umask 077
mkdir -p "$OUT"

finish() {
  local code=$1
  tar -czf "$OUT/logs.tar.gz.part" -C / \
    "${OUT#/}/runner.log" \
    $( [ -d "$RUNNER_DIR/_diag" ] && printf '%s' "${RUNNER_DIR#/}/_diag" ) 2>/dev/null
  mv -f "$OUT/logs.tar.gz.part" "$OUT/logs.tar.gz"
  printf '%s\n' "$code" >"$OUT/exit.part"
  mv -f "$OUT/exit.part" "$OUT/exit"
  exit "$code"
}

if [ ! -s "$JIT_FILE" ]; then
  printf 'arca-job: no runner configuration at %s\n' "$JIT_FILE" >"$OUT/runner.log"
  finish 90
fi
jit=$(cat "$JIT_FILE")
rm -f "$JIT_FILE"

if [ ! -x "$RUNNER_DIR/run.sh" ]; then
  printf 'arca-job: no runner at %s\n' "$RUNNER_DIR" >"$OUT/runner.log"
  finish 91
fi

# The work folder the configuration names (/home/runner/work) belongs to the runner user, as on GitHub.
install -d -o "$RUNNER_USER" -g "$(id -gn "$RUNNER_USER")" -m 0755 /home/runner/work

printf 'arca-job: runner starting at %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"$OUT/runner.log"
ACTIONS_RUNNER_INPUT_JITCONFIG="$jit" runuser -l "$RUNNER_USER" \
  -w ACTIONS_RUNNER_INPUT_JITCONFIG \
  -c "cd '$RUNNER_DIR' && exec ./run.sh" >>"$OUT/runner.log" 2>&1
code=$?
unset jit
printf 'arca-job: runner exited %s at %s\n' "$code" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >>"$OUT/runner.log"
finish "$code"

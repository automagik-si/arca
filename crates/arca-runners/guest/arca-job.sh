#!/bin/bash
# arca-job: runs one GitHub Actions runner, once, in a job machine Arca made for it.
#
# Arca writes this file into the machine and starts it as root through the Incus agent, after it wrote the runner's
# just-in-time configuration to /run/arca-job-jit (memory, mode 0600). It:
# 1. stops the image's own arca-runner.service, which waits at boot for a configuration handed over another way;
# 2. reads the configuration and deletes the file;
# 3. starts the runner as the runner user in a login session (so the limits apply) with /etc/environment loaded, as on
#    GitHub, with the configuration in the runner's environment only (never on a command line, never on disk again);
# 4. when the runner exits (a just-in-time runner takes exactly one job), packs its output and its _diag folder into
#    /var/log/arca-job/logs.tar.gz and writes the runner's exit code to /var/log/arca-job/exit, which Arca reads back
#    before it destroys the machine.
#
# The runner program is started directly (bin/Runner.Listener run), not through its run.sh: run.sh starts the runner
# again for ever after a "retryable" exit (a configuration it cannot use is one) and answers 0 for most failures. Here
# an update exit (3, 4) starts it again, as run.sh does, up to three times; a retryable exit (2) starts it at most
# ARCA_RUNNER_TRIES times in all (default 2); any other exit code is the runner's answer and is kept.
#
# ARCA_RUNNER_DIR names the runner's folder, ARCA_WORK_FOLDER the work folder its configuration names, and
# ARCA_RUNNER_USER its user. Arca sets the first two from the pool's settings; the defaults are GitHub's.
set -u

RUNNER_DIR=${ARCA_RUNNER_DIR:-/home/runner/actions-runner}
WORK_FOLDER=${ARCA_WORK_FOLDER:-/home/runner/work}
RUNNER_USER=${ARCA_RUNNER_USER:-runner}
TRIES=${ARCA_RUNNER_TRIES:-2}
JIT_FILE=/run/arca-job-jit
OUT=/var/log/arca-job

umask 077
mkdir -p "$OUT"

say() {
  printf 'arca-job: %s at %s\n' "$1" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >>"$OUT/runner.log"
}

finish() {
  local code=$1
  local parts=("${OUT#/}/runner.log")
  [ -d "$RUNNER_DIR/_diag" ] && parts+=("${RUNNER_DIR#/}/_diag")
  tar -czf "$OUT/logs.tar.gz.part" -C / "${parts[@]}" 2>/dev/null
  mv -f "$OUT/logs.tar.gz.part" "$OUT/logs.tar.gz"
  printf '%s\n' "$code" >"$OUT/exit.part"
  mv -f "$OUT/exit.part" "$OUT/exit"
  exit "$code"
}

: >"$OUT/runner.log"
# The image's own one-job service would wait for ever for a configuration that never comes its way.
systemctl stop arca-runner.service >/dev/null 2>&1 || true

if [ ! -s "$JIT_FILE" ]; then
  say "no runner configuration at $JIT_FILE"
  finish 90
fi
jit=$(cat "$JIT_FILE")
rm -f "$JIT_FILE"

if [ ! -x "$RUNNER_DIR/bin/Runner.Listener" ]; then
  say "no runner at $RUNNER_DIR"
  finish 91
fi

# The work folder the configuration names belongs to the runner user, as on GitHub.
install -d -o "$RUNNER_USER" -g "$(id -gn "$RUNNER_USER")" -m 0755 "$WORK_FOLDER"

tries=0 updates=0
while :; do
  say "runner starting"
  # The folder reaches the login shell as a variable, never spliced into the command, so any path works.
  # shellcheck disable=SC2016 # the login shell expands it
  ACTIONS_RUNNER_INPUT_JITCONFIG="$jit" ARCA_RUNNER_DIR="$RUNNER_DIR" runuser -l "$RUNNER_USER" \
    -w ACTIONS_RUNNER_INPUT_JITCONFIG,ARCA_RUNNER_DIR \
    -c 'set -a; [ -r /etc/environment ] && . /etc/environment; set +a
        cd "$ARCA_RUNNER_DIR" && exec ./bin/Runner.Listener run' >>"$OUT/runner.log" 2>&1
  code=$?
  say "runner exited $code"
  case $code in
    2)
      tries=$((tries + 1))
      [ "$tries" -ge "$TRIES" ] && break
      sleep 5
      ;;
    3 | 4)
      # The runner updated itself: wait for the update to finish, as run.sh does, and start the new one.
      updates=$((updates + 1))
      [ "$updates" -gt 3 ] && break
      for _ in $(seq 30); do
        [ -f "$RUNNER_DIR/update.finished" ] && { rm -f "$RUNNER_DIR/update.finished"; break; }
        sleep 1
      done
      ;;
    *) break ;;
  esac
done
unset jit
finish "$code"

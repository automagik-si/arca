# The postgres-autopg service's entrypoint (templates/postgres-autopg.json reads it in, put on one line by services.ts
# `scriptArg`, as `sh -ec <it> postgres-autopg`; arca-demo-automagik-omni Group 5, coordinator rulings 1 and 3). It runs
# as the image's own user (autopg) in ghcr.io/automagik-si/autopg, whose `autopg postmaster` alone listens on loopback
# only, with initdb's loopback-only pg_hba.conf and no database but postgres. So it:
# - writes $AUTOPG_CONFIG_DIR/settings.json so postgres listens on every address (postgres._extra.listen_addresses:
#   autopg reads it from that file only, no variable);
# - starts the provisioner in the background, then execs the image's own command, `autopg postmaster`, as the service;
# - the provisioner waits for the Unix socket, then makes role `app` (LOGIN and REPLICATION, so a consumer can create
#   its own logical slot, no other right, password POSTGRES_PASSWORD,
#   the service's write-only secret) and database `app` owned by it, with schema public its own, without logging the
#   statement; appends one pg_hba.conf rule, `host app app samenet scram-sha-256` (the service subnet: the NIC ACL decides
#   which consumer reaches 5432), and reloads. The superuser keeps initdb's loopback-only rules (autopg's own admin pool
#   uses it with its built-in password). It runs again at every start: the role's password follows the secret.
# One database and role per postgres service (ruling 3). A provisioning failure stops the postmaster, so the service
# stops and its log (service_logs) says why. Every name here is fixed; only the password comes from the environment.
set -eu
DATA=/var/lib/autopg/data/pgdata
SOCK=/var/run/autopg
PORT=5432
RULE='host app app samenet scram-sha-256'

: "${POSTGRES_PASSWORD:?POSTGRES_PASSWORD is not set: this service was created from a template that did not make it (arca templates install, then create it again; or service_secret_set)}"
case "$POSTGRES_PASSWORD" in
  *[!A-Za-z0-9._~-]*)
    echo 'postgres-autopg: POSTGRES_PASSWORD must be letters, digits and . _ ~ - only: it goes into DATABASE_URL as it is' >&2
    exit 64
    ;;
esac
printf '{"postgres":{"_extra":{"listen_addresses":"*"}}}\n' > "${AUTOPG_CONFIG_DIR:-/var/lib/autopg}/settings.json"

provision() {
  i=0
  until pg_isready -q -h "$SOCK" -p "$PORT"; do
    i=$((i + 1))
    if [ "$i" -gt 150 ]; then
      echo 'postgres-autopg: postgres did not accept connections within 300 s' >&2
      return 1
    fi
    sleep 2
  done
  psql -X -q -v ON_ERROR_STOP=1 -h "$SOCK" -p "$PORT" -U postgres -d postgres -v pw="$POSTGRES_PASSWORD" <<'SQL'
SET log_statement = 'none';
SELECT 'CREATE ROLE app LOGIN' WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'app') \gexec
SELECT format('ALTER ROLE app LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE REPLICATION PASSWORD %L', :'pw') \gexec
SELECT 'CREATE DATABASE app OWNER app' WHERE NOT EXISTS (SELECT 1 FROM pg_database WHERE datname = 'app') \gexec
\connect app
ALTER SCHEMA public OWNER TO app;
SQL
  if ! grep -qxF "$RULE" "$DATA/pg_hba.conf"; then
    printf '# postgres-autopg (templates/postgres-autopg.sh): the app role from the service subnet\n%s\n' "$RULE" >> "$DATA/pg_hba.conf"
  fi
  psql -X -q -v ON_ERROR_STOP=1 -h "$SOCK" -p "$PORT" -U postgres -d postgres -c 'SELECT pg_reload_conf()' > /dev/null
  echo "postgres-autopg: database app and role app ready on port $PORT"
}

# $$ is this shell, which the exec below turns into the postmaster.
main=$$
(provision || { echo 'postgres-autopg: provisioning failed; stopping the postmaster' >&2; kill -TERM "$main"; }) &
exec autopg postmaster --port "$PORT" --data "$DATA" --socket-dir "$SOCK" --log info

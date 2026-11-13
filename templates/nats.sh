# The entrypoint of the nats service (templates/nats.json reads it in, put on one line by service::entrypoint, as
# `sh -ec <it> nats`). It runs in the public nats:2.14.10-alpine image, as root, writes the server's configuration and
# then execs the server. Every setting here is fixed; only what the environment gives changes the file:
# - NATS_DOMAIN (the service's name): the JetStream domain, and the server's name, which is unique within a domain;
# - NATS_LEAF_PASSWORD (a secret the template makes): the password of the one user, `leaf`, that the leaf node listener
#   accepts. It is letters, digits and . _ ~ - only, so it can go into a leaf's URL as it is.
# What the file says:
# - client port 4222 with no login: a service network reaches it only through a link, which is how a client such as
#   Omni's, which sends no user or password, works. NATS_URL is nats://HOST:4222 and carries no credential;
# - monitoring on 8222 (http, no login: it shows how the server is, never a message);
# - JetStream on the data volume, store_dir /data/jetstream, so what the service stores survives an update;
# - sync_interval always: the server syncs the file to disk before it acknowledges each message. It costs throughput,
#   and it is the point of this template (a stream created with persist_mode async turns it off for that stream). The
#   image is NATS 2.14.10; the promise needs 2.14.6 or later, where a block compaction (a delete, a work-queue or
#   interest stream) honours it: 2.10 and earlier compacted without a sync, so a power loss in the next two minutes
#   could lose messages that were acknowledged;
# - max_file_store 8 GiB, under the volume's 10 GiB (the template states the volume's size; keep this below it), so a
#   full store is refused by JetStream and never fills the volume the server's own files live on;
# - a leaf node listener on 7422 that takes only the user `leaf`. No link reaches it yet: a link opens only 4222 (a
#   tenant's front service and a function may still reach it; see docs/OPERATING.md).
# The configuration holds the leaf password, so it is written with mode 600 and under /tmp, not on the data volume.
# Discard new is not set here: it is a setting of each stream, made by whoever creates the stream (docs/OPERATING.md,
# "A message bus: nats").
set -eu
# Where things go. The three ARCA_NATS_* names are for the tests; the image sets none of them. (The image does set
# NATS_SERVER, to its version, 2.14.10: reading that name as a path made the exec fail, so none of the image's names
# is read here.)
STORE=${ARCA_NATS_STORE_DIR:-/data/jetstream}
CONF=${ARCA_NATS_CONF:-/tmp/nats-server.conf}
SERVER=${ARCA_NATS_SERVER:-/usr/local/bin/nats-server}
MAX_FILE_STORE=8589934592

: "${NATS_LEAF_PASSWORD:?NATS_LEAF_PASSWORD is not set: this service was created from a template that did not make it (arca templates install, then create it again; or service_secret_set)}"
case "$NATS_LEAF_PASSWORD" in
  *[!A-Za-z0-9._~-]*)
    echo 'nats: NATS_LEAF_PASSWORD must be letters, digits and . _ ~ - only' >&2
    exit 64
    ;;
esac
: "${NATS_DOMAIN:?NATS_DOMAIN is not set: the template gives it the service name}"
case "$NATS_DOMAIN" in
  *[!a-z0-9-]* | -*)
    echo 'nats: NATS_DOMAIN must be lower-case letters, digits and - only, and not start with -' >&2
    exit 64
    ;;
esac

mkdir -p "$STORE"
umask 077
cat > "$CONF" <<CONFIG
# Written by templates/nats.sh at every start; changes here are lost.
server_name: "$NATS_DOMAIN"
port: 4222
http: "0.0.0.0:8222"
jetstream {
  store_dir: "$STORE"
  domain: "$NATS_DOMAIN"
  max_file_store: $MAX_FILE_STORE
  sync_interval: always
}
leafnodes {
  listen: "0.0.0.0:7422"
  authorization {
    user: "leaf"
    password: "$NATS_LEAF_PASSWORD"
  }
}
CONFIG
echo "nats: $NATS_DOMAIN, JetStream under $STORE, client port 4222, leaf port 7422, monitoring 8222"
exec "$SERVER" -c "$CONF"

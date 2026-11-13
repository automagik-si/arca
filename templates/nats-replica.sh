# The entrypoint of the nats-replica service (templates/nats-replica.json reads it in, put on one line by
# service::entrypoint, as `sh -ec <it> nats-replica`). It runs in the public nats:2.14.10-alpine image, as root, writes
# the server's configuration and then execs the server. It is nats.sh with one change: where nats.sh has a leaf node
# listener, this has one leaf node remote, which dials the leaf node port of a nats-primary on another node. Every
# setting is fixed; only what the environment gives changes the file:
# - NATS_DOMAIN (the service's name): the JetStream domain, and the server's name. It must not be the primary's
#   service name, which is the primary's domain: across a leaf node two servers of one domain answer the same API
#   subject and no mirror forms;
# - NATS_LEAF_REMOTE (a secret the owner sets): the primary's leaf node port as ADDRESS:PORT, an address the node's
#   kernel.serviceDestinations lets this service reach on that port;
# - NATS_LEAF_PASSWORD (a secret the owner sets): the primary's own NATS_LEAF_PASSWORD, the password of the user
#   `leaf` there. It is letters, digits and . _ ~ - only, so it goes into the remote's URL as it is.
# What the file says:
# - client port 4222 with no login and monitoring on 8222 (http, no login), as nats.sh says and for the same reasons;
# - JetStream on the data volume (store_dir /data/jetstream), sync_interval always and max_file_store 8 GiB under the
#   volume's 10 GiB, as nats.sh: a mirror is a stream like any other, and it is what is left if the primary is lost;
# - no leaf node listener: nothing dials a replica;
# - one leaf node remote, nats-leaf://ADDRESS:PORT with the user `leaf` and the password in it. The server dials it
#   again every second while it does not answer, and meanwhile starts and serves its own streams. Over that link a
#   mirror here reads a stream there through the primary's JetStream API, $JS.<primary's domain>.API. The remote
#   ignores the servers the primary tells it of (ignore_discovered_servers): the primary listens on every address of
#   its container, so it names that container's address on its own node's service network, which this server would
#   otherwise dial first, with the password, after the first disconnect. Only the configured address is ever dialled.
# The configuration holds the password, so it is written with mode 600 and under /tmp, not on the data volume. No stream
# is made here: whoever runs the pair makes each mirror with the nats CLI (docs/OPERATING.md, "A cold replica of nats on
# a second node").
set -eu
# Where things go, as in nats.sh. The three ARCA_NATS_* names are for the tests; the image sets none of them, and none of
# the image's own names (it sets NATS_SERVER, to its version) is read here.
STORE=${ARCA_NATS_STORE_DIR:-/data/jetstream}
CONF=${ARCA_NATS_CONF:-/tmp/nats-server.conf}
SERVER=${ARCA_NATS_SERVER:-/usr/local/bin/nats-server}
MAX_FILE_STORE=8589934592

: "${NATS_LEAF_PASSWORD:?NATS_LEAF_PASSWORD is not set: set it to the primary's own NATS_LEAF_PASSWORD (service_secret_set), then instance_start}"
case "$NATS_LEAF_PASSWORD" in
  *[!A-Za-z0-9._~-]*)
    echo 'nats-replica: NATS_LEAF_PASSWORD must be letters, digits and . _ ~ - only' >&2
    exit 64
    ;;
esac
: "${NATS_LEAF_REMOTE:?NATS_LEAF_REMOTE is not set: set it to the primary's leaf node port as ADDRESS:PORT (service_secret_set), then instance_start}"
# An address or a host name, one colon, and a port from 1 to 65535 written plainly: nothing that could end the URL's
# quotes or add a user, a path or a second address.
case "$NATS_LEAF_REMOTE" in
  *[!A-Za-z0-9.:-]* | [.:-]* | *:*:*) REMOTE_PORT= ;;
  *:*) REMOTE_PORT=${NATS_LEAF_REMOTE##*:} ;;
  *) REMOTE_PORT= ;;
esac
case "$REMOTE_PORT" in
  '' | 0* | *[!0-9]* | ??????*) REMOTE_PORT= ;;
esac
if [ -z "$REMOTE_PORT" ] || [ "$REMOTE_PORT" -gt 65535 ]; then
  echo 'nats-replica: NATS_LEAF_REMOTE must be ADDRESS:PORT, the leaf node port of the primary: letters, digits, dots and dashes, one colon, and a port from 1 to 65535' >&2
  exit 64
fi
: "${NATS_DOMAIN:?NATS_DOMAIN is not set: the template gives it the service name}"
case "$NATS_DOMAIN" in
  *[!a-z0-9-]* | -*)
    echo 'nats-replica: NATS_DOMAIN must be lower-case letters, digits and - only, and not start with -' >&2
    exit 64
    ;;
esac
# The user and the password of the remote, put together here and not inside the URL below.
LEAF_LOGIN="leaf:$NATS_LEAF_PASSWORD"

mkdir -p "$STORE"
umask 077
cat > "$CONF" <<CONFIG
# Written by templates/nats-replica.sh at every start; changes here are lost.
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
  remotes: [
    {
      url: "nats-leaf://$LEAF_LOGIN@$NATS_LEAF_REMOTE"
      ignore_discovered_servers: true
    }
  ]
}
CONFIG
echo "nats-replica: $NATS_DOMAIN, JetStream under $STORE, client port 4222, monitoring 8222, one leaf node remote to the primary"
exec "$SERVER" -c "$CONF"

# shellcheck shell=sh
# The entrypoint of the tailscale connector (templates/tailscale.json reads it in, put on one line by
# service::entrypoint, as `sh -ec <it> tailscale`). It runs in the public tailscale/tailscale image (Tailscale 1.104.7),
# as root, writes one serve config and then execs the image's own containerboot, which starts tailscaled, logs the node
# in and applies that serve config once the node is up.
#
# What the environment gives, from the template and the link:
# - ARCA_EDGE_TARGET: the linked edge's ADDRESS:PORT, provided by service_connect;
# - TS_TARGET (an optional secret the owner sets): overrides the link's target with ADDRESS:PORT, or
#   NAME@ADDRESS:PORT, where NAME is the node's tailnet name in place of the service's own;
# - TS_AUTHKEY (an optional secret the owner sets): the auth key. It is written to a file of its own, mode 600,
#   under /run (which Arca keeps in memory), and containerboot is given TS_AUTHKEY=file:<that file>: `tailscale up` reads the key from
#   the file (Tailscale 1.104.7, cmd/tailscale/cli/up.go, resolveValueFromFile), so the key is never on a command line
#   and never in containerboot's own environment;
# - TS_HOSTNAME (the service's name), TS_STATE_DIR (/data, the service's data volume), TS_USERSPACE and TS_AUTH_ONCE
#   (both true): read by containerboot as they are. Userspace networking needs no tun device and no capability. With
#   TS_AUTH_ONCE the key is used only while the node has no login: once its state is on the data volume, every later
#   start or reconfigure logs in from that state and never uses the key again, so a single-use key is enough.
#
# The serve config forwards TCP 443 on the node's tailnet address to the chosen target as raw TCP: no TerminateTLS,
# so TLS is the target's own. ProxyProtocol 2 puts a PROXY protocol v2 header first, so the target sees each tailnet client's own
# address (Tailscale 1.104.7, ipn/serve.go, TCPPortHandler.ProxyProtocol; ipn/ipnlocal/serve.go,
# forwardTCPWithProxyProtocol). tailscaled dials the target from the container's own network (SystemDial), so a private
# address on the service network is reached; the serve config is given as a file, so no `tailscale serve` runs.
# Nothing else is served: a connection to any other port of the node's tailnet address goes to that port on the
# container's loopback, where nothing listens. The file is written under /tmp at every start, never on the data volume.
set -eu
# Where things go. The three ARCA_TS_* names are for the tests; the image sets none of them.
SERVE_DIR=${ARCA_TS_SERVE_DIR:-/tmp/arca-tailscale}
KEY_DIR=${ARCA_TS_KEY_DIR:-/run/arca-tailscale}
BOOT=${ARCA_TS_CONTAINERBOOT:-/usr/local/bin/containerboot}
# The owner taps sign-in just after approving: keep an unclaimed link open briefly. A restart retries it.
# Every keyless start gets this default: a saved state file alone is no proof that its login is still valid.
SIGN_IN_BOOT_TIMEOUT=10m
if [ -z "${TS_AUTHKEY:-}" ]; then
  TS_BOOT_TIMEOUT=${TS_BOOT_TIMEOUT-$SIGN_IN_BOOT_TIMEOUT}
  export TS_BOOT_TIMEOUT
fi

TARGET=${TS_TARGET:-${ARCA_EDGE_TARGET:-}}
if [ -z "$TARGET" ]; then
  echo 'tailscale: neither TS_TARGET nor ARCA_EDGE_TARGET is set: link this service to the edge (service_connect), or set TS_TARGET to ADDRESS:PORT (service_secret_set), then instance_start' >&2
  exit 64
fi
# An optional tailnet name before an @: 1 to 63 lower-case letters, digits and dashes, not starting or ending with a
# dash, as a DNS label is.
case "$TARGET" in
  *@*)
    NAME=${TARGET%%@*}
    TARGET=${TARGET#*@}
    case "$NAME" in
      '' | *[!a-z0-9-]* | -* | *- | ????????????????????????????????????????????????????????????????*)
        echo 'tailscale: the name before @ in TS_TARGET must be 1 to 63 lower-case letters, digits and dashes, not starting or ending with a dash' >&2
        exit 64
        ;;
    esac
    TS_HOSTNAME=$NAME
    export TS_HOSTNAME
    ;;
esac
# An address or a host name, one colon, and a port from 1 to 65535 written plainly: nothing that could end the JSON
# string, name a unix socket (tailscaled reads a target that starts with unix: as one) or add a second target.
case "$TARGET" in
  *[!A-Za-z0-9.:-]* | [.:-]* | *:*:* | unix:*) PORT= ;;
  *:*) PORT=${TARGET##*:} ;;
  *) PORT= ;;
esac
case "$PORT" in
  '' | 0* | *[!0-9]* | ??????*) PORT= ;;
esac
if [ -z "$PORT" ] || [ "$PORT" -gt 65535 ]; then
  echo 'tailscale: TS_TARGET must be ADDRESS:PORT or NAME@ADDRESS:PORT, the linked service the tailnet port 443 goes to: letters, digits, dots and dashes, one colon, and a port from 1 to 65535' >&2
  exit 64
fi

mkdir -p "$SERVE_DIR"
printf '{"TCP":{"443":{"TCPForward":"%s","ProxyProtocol":2}}}\n' "$TARGET" > "$SERVE_DIR/serve.json"
export TS_SERVE_CONFIG="$SERVE_DIR/serve.json"
# The key, when there is one, by file. A node that logs in from its state needs none.
case "${TS_AUTHKEY:-}" in
  '' | file:*) ;;
  *)
    mkdir -p "$KEY_DIR"
    (umask 077 && printf '%s\n' "$TS_AUTHKEY" > "$KEY_DIR/authkey")
    TS_AUTHKEY="file:$KEY_DIR/authkey"
    export TS_AUTHKEY
    ;;
esac
echo "tailscale: node ${TS_HOSTNAME:-}, state in ${TS_STATE_DIR:-}, tailnet port 443 forwarded as raw TCP with PROXY protocol v2 to the linked target"
exec "$BOOT"

# The entrypoint of the tailscale connector (templates/tailscale.json reads it in, put on one line by
# service::entrypoint, as `sh -ec <it> tailscale`). It runs in the public tailscale/tailscale image (Tailscale 1.104.7),
# as root, writes one serve config and then execs the image's own containerboot, which starts tailscaled, logs the node
# in and applies that serve config once the node is up.
#
# What the environment gives, every name from the template:
# - TS_TARGET (a secret the owner sets): ADDRESS:PORT of the one linked service that port 443 forwards to, or
#   NAME@ADDRESS:PORT, where NAME is the node's tailnet name in place of the service's own. Every secret a template's
#   environment names must be set before a start, and a secret is at least 8 characters, so an optional name of its
#   own cannot be a second secret: it rides on the one value that is always set;
# - TS_AUTHKEY (a secret the owner sets): the auth key, read by containerboot;
# - TS_HOSTNAME (the service's name), TS_STATE_DIR (/data, the service's data volume), TS_USERSPACE and TS_AUTH_ONCE
#   (both true): read by containerboot as they are. Userspace networking needs no tun device and no capability. With
#   TS_AUTH_ONCE the key is used only while the node has no login: once its state is on the data volume, every later
#   start or reconfigure logs in from that state and never uses the key again, so a single-use key is enough.
#
# The serve config forwards TCP 443 on the node's tailnet address to TS_TARGET as raw TCP: no TerminateTLS, so TLS is
# the target's own. ProxyProtocol 2 puts a PROXY protocol v2 header first, so the target sees each tailnet client's own
# address (Tailscale 1.104.7, ipn/serve.go, TCPPortHandler.ProxyProtocol; ipn/ipnlocal/serve.go,
# forwardTCPWithProxyProtocol). tailscaled dials the target from the container's own network (SystemDial), so a private
# address on the service network is reached; only the CLI keeps `tailscale serve` to loopback targets. Nothing else is
# served: a connection to any other port of the node's tailnet address goes to that port on the container's loopback,
# where nothing listens. The file is written under /tmp at every start, never on the data volume.
set -eu
# Where things go. The two ARCA_TS_* names are for the tests; the image sets neither.
SERVE_DIR=${ARCA_TS_SERVE_DIR:-/tmp/arca-tailscale}
BOOT=${ARCA_TS_CONTAINERBOOT:-/usr/local/bin/containerboot}

: "${TS_TARGET:?TS_TARGET is not set: set it to the linked service's ADDRESS:PORT (service_secret_set), then instance_start}"
# An optional tailnet name before an @: 1 to 63 lower-case letters, digits and dashes, not starting or ending with a
# dash, as a DNS label is.
TARGET=$TS_TARGET
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
# string, name a unix socket or add a second target.
case "$TARGET" in
  *[!A-Za-z0-9.:-]* | [.:-]* | *:*:*) PORT= ;;
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
echo "tailscale: node ${TS_HOSTNAME:-}, state in ${TS_STATE_DIR:-}, tailnet port 443 forwarded as raw TCP with PROXY protocol v2 to the linked target"
exec "$BOOT"

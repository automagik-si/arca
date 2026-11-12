#!/bin/sh
# Install Automagik Arca as a read-only node for the user who runs this, in one command.
#
#   sh install.sh                      install, or upgrade in place, the release named below
#   sh install.sh --version <version>  the same, for another release
#   sh install.sh --no-door            the same, and the node opens no TCP port: no HTTP MCP door, now or on a later run
#   sh install.sh --door               the same, and the door is opened again after an earlier --no-door
#   sh install.sh --enable-changes     the same, and then the node may make changes, each only after the owner
#                                      approves it with a passkey: it runs `arca init --enable-changes --yes`
#   sh install.sh --uninstall          remove the program and the service this script installed
#   sh install.sh --help
#
# It leaves what the quickstart, docs/QUICKSTART.md ("By hand"), leaves:
#
#   ~/.local/bin/arca                        the program, checked against the release's SHA256SUMS
#   ~/.config/arca/node.json                 the quickstart's smallest config; it names no kernel project, so the node
#                                            only reads: it sends Incus nothing but GET. It opens the HTTP MCP door
#                                            on this machine only: 127.0.0.1, port 7471 or the next free one
#   ~/.config/arca/tokens/                   mode 0700; holds the hash of the one token
#   ~/.config/arca/assistant-1.secret        one `viewer` token, readable by this user only; never printed
#   ~/.local/state/arca-node/                mode 0700; the node's state
#   ~/.config/systemd/user/arca.service      where `systemctl --user` answers: the node as a service, enabled, started
#   ~/.local/state/arca-install/             this script's own note of the program it installed, for --uninstall
#
# What it never does: it never runs sudo or su and refuses to run as root, never writes outside this user's home and
# runtime directory (XDG_RUNTIME_DIR), never changes a config it did not write, never prints the token, never reads
# its standard input (so `curl ... | sh` works), never puts an older program over a newer one unless `--version`
# asks, and never repairs the machine: when something is missing it says what and how to fix it, and stops before it
# has changed anything.
#
# The one config it writes again is its own, letter for letter, from an earlier run, and only for the door: to add
# it when this run replaces a program from before the door with one that has it, to do what --door or --no-door
# asks, or to take the key out for an older program, which refuses a config that names it.
#
# The owner's no is kept. --no-door writes `"loopback": null` into the config, and a run without a flag never opens
# a door in a config that says null, nor in one that has been without a door under a program that has one.
#
# It starts a node only for a config it wrote, or through a service it wrote earlier, and only where it can ask the
# node afterwards what it is. For any other config it installs the program, starts nothing and says so.
#
# Templates. A node gets its service templates from `arca init`, not from this script. Where the node this script
# runs as a service already has them, an upgrade runs `arca templates install` for it: a template that is an
# earlier release's own file, byte for byte, becomes this release's, and the report names it. A template the owner
# edited is kept, and the report names it too, with the one command that replaces it. A node with no templates gets
# none here.
#
# How it ends:
#   0   the node runs and answered `arca_status`, and Incus answers the node
#   1   refused or failed; the words say what, and a refusal has changed nothing
#   3   the program is installed and this script started no node; the words say why and give the one command
#
# A second run downloads the release again, replaces the program in place and restarts the service.
#
# Four values come from the environment: INCUS_SOCKET, the Incus socket when it is not /var/lib/incus/unix.socket
# (the name the Incus client itself reads); ARCA_INSTALL_WAIT, how many seconds to wait for the started node's first
# answer (30); and, for the tests, ARCA_INSTALL_RUN_ROOT, where per-user runtime directories live (/run/user), and
# ARCA_INSTALL_TCP_TABLES, the files that list this machine's TCP sockets (/proc/net/tcp and /proc/net/tcp6).
set -eu

# The release this script installs. `--version` names another. The script never asks GitHub which release is the
# newest: each release carries its own copy of this script, with this line set to that release.
ARCA_VERSION=0.13.0

RELEASES=https://github.com/automagik-si/arca/releases
GUIDE=https://github.com/automagik-si/arca/blob/dev/docs/OPERATING.md
UNIT=arca.service
UNIT_MARK='# Automagik Arca: written by scripts/install.sh.'
# The two units the operating guide installs by hand. A user who has them already runs the node as a service.
GUIDE_UNITS='arca-watch.service arca-shim.service'
TOKEN_ID=assistant-1
# The first release whose program reads `listen.loopback`, the HTTP MCP door. An older program refuses a config that
# names the key, so for one this script writes the config without it.
DOOR_SINCE=0.11.0
# The ports the door may take, in this order: the first that nothing on this machine listens on.
DOOR_PORTS='7471 7472 7473 7474 7475 7476 7477 7478 7479 7480'
# The oldest glibc the published program starts on.
GLIBC_MAJOR=2
GLIBC_MINOR=34
# How this script ends when the program is installed and it started no node.
NOT_STARTED=3
# Every program this script starts, but for `arca` itself and `systemctl`, which is used only where it answers.
INSTALL_PROGRAMS='uname id getconf curl sha256sum mktemp mkdir chmod mv rm cat ls find sleep expr'

tmp=
tmp_file=
problems=0

say() {
    printf '%s\n' "$*"
}

fail() {
    printf 'install: %s\n' "$1" >&2
    shift
    for line in "$@"; do
        printf '         %s\n' "$line" >&2
    done
    exit 1
}

# One thing that is missing, and how to fix it. The checks go on, so one run names everything.
problem() {
    problems=$((problems + 1))
    printf 'install: %s\n' "$1" >&2
    shift
    for line in "$@"; do
        printf '         %s\n' "$line" >&2
    done
}

usage() {
    cat <<EOF
Install Automagik Arca as a read-only node for this user.

  sh install.sh                      install, or upgrade in place, release $ARCA_VERSION
  sh install.sh --version <version>  the same, for another release (see $RELEASES)
  sh install.sh --uninstall          remove the program and the service; keep the config, the tokens and the state
  sh install.sh --help

It needs Linux on x86_64 with glibc $GLIBC_MAJOR.$GLIBC_MINOR or newer, curl, sha256sum, and an Incus this user can reach.
It never runs sudo, and it changes nothing outside this user's home and runtime directory.
A program from release $DOOR_SINCE on answers MCP clients of this machine at http://127.0.0.1:<port>/mcp (port
${DOOR_PORTS%% *}, or the next free one), with a token. It opens no address another machine can reach.

  sh install.sh --no-door            open no TCP port at all; a later run without a flag keeps it that way
  sh install.sh --door               open the door again
  sh install.sh --enable-changes     after installing, let the node make changes: what an assistant proposes runs
                                     only after you approve it with your passkey (it runs: arca init
                                     --enable-changes --yes, which names any token that does not wait for that).
                                     Without it the node only looks

It ends with 0 when the node runs and answered, with 1 when it refused or failed, and with $NOT_STARTED when the program
is installed and it started no node.
EOF
}

cleanup() {
    if [ -n "$tmp" ]; then
        rm -rf "${tmp:?}"
        tmp=
    fi
    if [ -n "$tmp_file" ]; then
        rm -f "${tmp_file:?}"
        tmp_file=
    fi
}

have() {
    command -v "$1" >/dev/null 2>&1
}

# Whether anything has this name, a link that points nowhere included.
exists() {
    [ -e "$1" ] || [ -L "$1" ]
}

number() {
    case $1 in
        '' | *[!0-9]*) return 1 ;;
    esac
    return 0
}

# A path this script may write into a JSON file and a systemd unit as it is: absolute, with no character that either
# would read as something else.
plain_path() {
    case $1 in
        /*) ;;
        *) return 1 ;;
    esac
    case $1 in
        *[[:space:]]* | *[[:cntrl:]]* | *\"* | *\'* | *\\* | *\$* | *%* | *\`*) return 1 ;;
    esac
    return 0
}

# Whether the directory $1 belongs to the user who runs this.
mine() {
    [ -n "$(find "$1/." -prune -user "$uid" 2>/dev/null)" ]
}

# Whether neither its group nor anyone else may write the directory $1.
closed() {
    [ -n "$(find "$1/." -prune ! -perm -020 ! -perm -002 2>/dev/null)" ]
}

# The string value of the first "key":"value" in one line of compact JSON.
json_text() {
    case $1 in
        *"\"$2\":\""*) ;;
        *) return 1 ;;
    esac
    json_rest=${1#*"\"$2\":\""}
    printf '%s\n' "${json_rest%%\"*}"
}

# The names in the list `"$2":[...]` of the one-line JSON $1, as words with commas: `a, b`. Fails when $1 has no such
# list; prints nothing for an empty one. The names are file names, which hold no comma and no quote.
json_list() {
    case $1 in
        *"\"$2\":["*) ;;
        *) return 1 ;;
    esac
    json_rest=${1#*"\"$2\":["}
    json_rest=${json_rest%%]*}
    json_names=
    while [ -n "$json_rest" ]; do
        json_item=${json_rest%%,*}
        json_item=${json_item#\"}
        json_item=${json_item%\"}
        json_names=${json_names:+$json_names, }$json_item
        case $json_rest in
            *,*) json_rest=${json_rest#*,} ;;
            *) json_rest= ;;
        esac
    done
    printf '%s\n' "$json_names"
}

# The version in the line `arca --version` prints: "Automagik Arca <version> (<commit>, <target>)".
said_version() {
    case $1 in
        'Automagik Arca '*) ;;
        *) return 1 ;;
    esac
    said=${1#Automagik Arca }
    printf '%s\n' "${said%% *}"
}

# Whether version $1 is newer than version $2, in the order of semantic versioning: the three numbers first; then a
# release is newer than its pre-releases; then the pre-release's parts, numbers as numbers and words as words. A
# version this cannot read is not newer.
newer() {
    newer_a=${1%%+*}
    newer_b=${2%%+*}
    a_core=${newer_a%%-*}
    b_core=${newer_b%%-*}
    a_pre=
    b_pre=
    case $newer_a in
        *-*) a_pre=${newer_a#*-} ;;
    esac
    case $newer_b in
        *-*) b_pre=${newer_b#*-} ;;
    esac
    for _part in 1 2 3; do
        a_part=${a_core%%.*}
        b_part=${b_core%%.*}
        number "$a_part" || return 1
        number "$b_part" || return 1
        [ "$a_part" -le "$b_part" ] || return 0
        [ "$a_part" -ge "$b_part" ] || return 1
        case $a_core in
            *.*) a_core=${a_core#*.} ;;
            *) a_core=0 ;;
        esac
        case $b_core in
            *.*) b_core=${b_core#*.} ;;
            *) b_core=0 ;;
        esac
    done
    if [ -z "$a_pre" ]; then
        [ -n "$b_pre" ]
        return
    fi
    [ -n "$b_pre" ] || return 1
    while :; do
        a_part=${a_pre%%.*}
        b_part=${b_pre%%.*}
        if [ "$a_part" != "$b_part" ]; then
            if number "$a_part" && number "$b_part"; then
                [ "$a_part" -gt "$b_part" ]
                return
            fi
            # A number sorts before a word.
            number "$a_part" && return 1
            number "$b_part" && return 0
            LC_ALL=C expr "x$a_part" \> "x$b_part" >/dev/null
            return
        fi
        case $a_pre in
            *.*) a_pre=${a_pre#*.} ;;
            *) a_pre= ;;
        esac
        case $b_pre in
            *.*) b_pre=${b_pre#*.} ;;
            *) b_pre= ;;
        esac
        # The one with parts left over is the newer one.
        [ -n "$a_pre" ] || return 1
        [ -n "$b_pre" ] || return 0
    done
}

user_systemd() {
    have systemctl && systemctl --user show-environment >/dev/null 2>&1 </dev/null
}

# The first of the operating guide's units this user has, if any.
guide_unit() {
    for guide_name in $GUIDE_UNITS; do
        if exists "$unit_dir/$guide_name"; then
            printf '%s\n' "$guide_name"
            return 0
        fi
    done
    return 1
}

# Whether the unit file at this script's name is one this script wrote.
unit_is_ours() {
    unit_first=
    read -r unit_first <"$unit_file" || true
    case $unit_first in
        "$UNIT_MARK"*) return 0 ;;
    esac
    return 1
}

# Sets XDG_RUNTIME_DIR when the shell has none (a coding agent's shell often has none) and the usual directory is
# there. Fails when there is no runtime directory to use; the caller says why.
find_runtime_dir() {
    if [ -n "${XDG_RUNTIME_DIR-}" ]; then
        [ -d "$XDG_RUNTIME_DIR" ]
        return
    fi
    [ -n "$uid" ] || return 1
    runtime_guess="${ARCA_INSTALL_RUN_ROOT:-/run/user}/$uid"
    [ -d "$runtime_guess" ] || return 1
    XDG_RUNTIME_DIR=$runtime_guess
    export XDG_RUNTIME_DIR
    runtime_derived=1
}

# The config of the quickstart (docs/QUICKSTART.md), for this user and this machine. $1 is the door: a port, for the
# HTTP door on 127.0.0.1 and that port; `null`, for the owner's no, which the config states; or nothing, for a config
# that does not name the key. The text that does not name it is, letter for letter, what every release of this
# script before DOOR_SINCE wrote.
config_text() {
    case ${1-} in
        '') config_listen="{ \"unix\": \"$node_socket\" }" ;;
        null) config_listen="{ \"unix\": \"$node_socket\", \"loopback\": null }" ;;
        *) config_listen="{ \"unix\": \"$node_socket\", \"loopback\": \"127.0.0.1:$1\" }" ;;
    esac
    cat <<EOF
{
  "stateDir": "$state_dir",
  "incus": { "socket": "$incus_socket" },
  "listen": $config_listen,
  "tokens": { "hashDir": "$config_dir/tokens" },
  "kernel": { "droiddProjects": [] }
}
EOF
}

# Whether a TCP socket of this machine listens on port $1, on any address, read from the kernel's own tables: no
# connection is made. A table that cannot be read lists nothing.
port_taken() {
    port_hex=$(printf '%04X' "$1")
    for tcp_table in $tcp_tables; do
        [ -r "$tcp_table" ] || continue
        while read -r _slot local_address _remote tcp_state _rest; do
            case $local_address in
                *":$port_hex") [ "$tcp_state" != 0A ] || return 0 ;;
            esac
        done <"$tcp_table"
    done
    return 1
}

# The door's port: the first of DOOR_PORTS that nothing listens on.
free_door_port() {
    for door_try in $DOOR_PORTS; do
        if ! port_taken "$door_try"; then
            printf '%s\n' "$door_try"
            return 0
        fi
    done
    return 1
}

# Whether the program of version $1 reads `listen.loopback`: release DOOR_SINCE or a later one. A version this cannot
# read, and a pre-release of DOOR_SINCE, do not.
version_has_door() {
    [ "$1" = "$DOOR_SINCE" ] || newer "$1" "$DOOR_SINCE"
}

# Whether the program this run installed reads the key: its own version line says which release it is.
program_has_door() {
    door_program=$(said_version "$is") || return 1
    version_has_door "$door_program"
}

# Writes the config for the door $1 (see config_text), in one step.
put_config() {
    tmp_file=$(mktemp "$config_dir/.node.json.XXXXXX")
    config_text "$1" >"$tmp_file"
    mv -f "${tmp_file:?}" "${config:?}"
    tmp_file=
}

# ----------------------------------------------------------------------------------------------------------------------
# 1. The checks. Nothing is written before every one of them has passed.
# ----------------------------------------------------------------------------------------------------------------------

check_machine() {
    for program in $INSTALL_PROGRAMS; do
        have "$program" || problem "the program \`$program\` is not on PATH." \
            "Install it with the machine's package manager, then run this again."
    done

    if have uname; then
        system=$(uname -s)
        machine=$(uname -m)
        [ "$system" = Linux ] || problem "this is $system, and Arca's published program is for Linux only." \
            "Run this on a Linux machine with Incus."
        [ "$machine" = x86_64 ] || problem "this machine is $machine, and Arca's published program is for x86_64 only." \
            "Run this on an x86_64 machine, or build Arca from source (docs/BUILDING.md, \"Build and test\")."
    fi

    glibc=
    if have getconf; then
        glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null) || glibc=
    fi
    glibc=${glibc#glibc }
    glibc_major=${glibc%%.*}
    glibc_minor=${glibc#*.}
    glibc_minor=${glibc_minor%%.*}
    case "$glibc_major.$glibc_minor" in
        . | .* | *. | *[!0-9.]*)
            problem "this system does not say which glibc it has (\`getconf GNU_LIBC_VERSION\` gave no version)." \
                "Arca's published program needs glibc $GLIBC_MAJOR.$GLIBC_MINOR or newer: Enterprise Linux 9, Ubuntu 22.04, Debian 12, or newer." \
                "A system built on another C library, such as Alpine, cannot run it."
            glibc=
            ;;
        *)
            if [ "$glibc_major" -lt "$GLIBC_MAJOR" ] ||
                { [ "$glibc_major" -eq "$GLIBC_MAJOR" ] && [ "$glibc_minor" -lt "$GLIBC_MINOR" ]; }; then
                problem "this system has glibc $glibc, and Arca's published program needs $GLIBC_MAJOR.$GLIBC_MINOR or newer." \
                    "Use Enterprise Linux 9, Ubuntu 22.04, Debian 12, or newer."
            fi
            ;;
    esac
}

check_user() {
    if [ "$uid" = 0 ]; then
        problem "this is root, and Arca's node belongs to an ordinary user." \
            "A node that runs as root would hold more of the machine than it needs: it only has to reach Incus." \
            "Run this as the user who will own the node, in that user's own session, without sudo."
    fi
    if ! plain_path "$HOME"; then
        problem "the home directory \"$HOME\" is not a path this script can write into the config." \
            "It must be absolute and hold no space, quote, backslash, dollar, percent or backquote."
    elif [ ! -d "$HOME" ] || [ ! -w "$HOME" ]; then
        problem "the home directory $HOME is not a directory this user can write." \
            "Run this as the user who will own the node, with HOME set to that user's home."
    elif [ -n "$uid" ] && ! mine "$HOME"; then
        problem "the home directory $HOME belongs to another user, not to $user." \
            "Under sudo or su, HOME can stay the home of the user who called it, and the node would be installed there." \
            "Run this as the user who will own the node, in that user's own session."
    fi
}

check_runtime_dir() {
    if ! find_runtime_dir; then
        if [ -n "${XDG_RUNTIME_DIR-}" ]; then
            problem "XDG_RUNTIME_DIR is $XDG_RUNTIME_DIR, which is not a directory." \
                "The node's socket lives there. Unset it, or set it to this user's runtime directory, then run this again."
        else
            problem "XDG_RUNTIME_DIR is not set and ${ARCA_INSTALL_RUN_ROOT:-/run/user}/${uid:-<uid>} does not exist." \
                "The node's socket lives in this user's runtime directory, which the system makes when the user logs in." \
                "Log in as this user (a console or ssh session), or have the machine's administrator keep the" \
                "directory without a session (\`loginctl enable-linger $user\`), then run this again."
        fi
        return 0
    fi
    node_socket=$XDG_RUNTIME_DIR/arca.sock
    if ! plain_path "$XDG_RUNTIME_DIR"; then
        problem "the runtime directory \"$XDG_RUNTIME_DIR\" is not a path this script can write into the config." \
            "It must be absolute and hold no space, quote, backslash, dollar, percent or backquote."
    elif ! mine "$XDG_RUNTIME_DIR" || ! closed "$XDG_RUNTIME_DIR"; then
        problem "the runtime directory $XDG_RUNTIME_DIR is not this user's alone:" \
            "$(ls -ldn "$XDG_RUNTIME_DIR/." 2>/dev/null || echo "(it cannot be listed)")" \
            "The node's socket lives there, and whoever else may write the directory could put their own in its place." \
            "Set XDG_RUNTIME_DIR to this user's own runtime directory (mode 0700; /run/user/${uid:-<uid>} on most systems)."
    elif [ ! -w "$XDG_RUNTIME_DIR" ]; then
        problem "this user cannot write the runtime directory $XDG_RUNTIME_DIR." \
            "The node's socket lives there. Set XDG_RUNTIME_DIR to this user's own runtime directory."
    elif ! exists "$config" && [ "${#node_socket}" -gt 100 ]; then
        problem "the node's socket path $node_socket is longer than 100 characters." \
            "A unix socket path cannot be that long. Set XDG_RUNTIME_DIR to a shorter directory."
    fi
}

check_incus() {
    if ! plain_path "$incus_socket"; then
        problem "the Incus socket \"$incus_socket\" (INCUS_SOCKET) is not a path this script can write into the config." \
            "It must be absolute and hold no space, quote, backslash, dollar, percent or backquote."
    elif [ ! -S "$incus_socket" ]; then
        problem "there is no Incus socket at $incus_socket." \
            "Arca reads the machine through Incus. Install Incus and start it" \
            "(https://linuxcontainers.org/incus/docs/main/installing/), then run this again." \
            "If Incus listens on another socket, name it: INCUS_SOCKET=<path> sh install.sh"
    elif [ ! -r "$incus_socket" ] || [ ! -w "$incus_socket" ]; then
        problem "the user $user may not open the Incus socket $incus_socket:" \
            "$(ls -l "$incus_socket" 2>/dev/null || echo "(it cannot be listed)")" \
            "Add $user to the group that owns the socket (\`incus-admin\` on most installs), then log in again so" \
            "the new group applies. That step is the machine's administrator's: usermod -aG incus-admin $user"
    elif have curl && ! curl -fsS --max-time 10 --unix-socket "$incus_socket" -o /dev/null http://incus/1.0 </dev/null 2>/dev/null; then
        problem "the Incus socket $incus_socket is there, and nothing answers on it." \
            "Start Incus (on most installs the machine's administrator runs \`systemctl start incus\`), then run this again."
    fi
}

check_units() {
    if guide_name=$(guide_unit); then
        problem "this user already runs Arca as a service set up by hand: $unit_dir/$guide_name is there." \
            "This script sets up a first read-only node, and it would replace the program that service runs." \
            "Upgrade that install by hand instead: step 1 of docs/QUICKSTART.md (\"By hand\"), then restart the service."
    elif exists "$unit_file" && ! unit_is_ours; then
        problem "$unit_file exists and this script did not write it." \
            "The installer's service has that name. Move that file away or rename your unit, then run this again."
    fi
}

# What is installed now. An older release is never put over a newer one unless `--version` asked for it.
check_installed() {
    was=
    was_version=
    if [ -x "$bin" ]; then
        was=$("$bin" --version 2>/dev/null </dev/null) || was=
        was_version=$(said_version "$was") || was_version=
    fi
    if [ -n "$was_version" ] && [ "$version_named" = 0 ] && newer "$was_version" "$version"; then
        problem "the program at $bin is $was_version, which is newer than $version, the release this script installs." \
            "This script does not put an older program over a newer one unless it is told to." \
            "To install $version all the same: sh install.sh --version $version"
    fi
}

# ----------------------------------------------------------------------------------------------------------------------
# 2. The program: one named release, checked against its SHA256SUMS, then put in place in one step.
# ----------------------------------------------------------------------------------------------------------------------

install_program() {
    asset="arca-$version-linux-x86_64"
    base="$RELEASES/download/v$version"

    mkdir -p "$bin_dir"
    # The download lands beside its destination: inside the home, and on the same file system, so the last step is a
    # rename and a running node keeps the program it started with.
    tmp=$(mktemp -d "$bin_dir/.arca-install.XXXXXX")

    say "Downloading $asset and SHA256SUMS from $base"
    for file in "$asset" SHA256SUMS; do
        # A download that stalls ends; it never hangs whoever waits for this script.
        curl -fsSL --proto '=https' --tlsv1.2 --retry 2 --connect-timeout 20 --max-time 900 \
            -o "$tmp/$file" "$base/$file" </dev/null ||
            fail "could not download $base/$file" \
                "Check that the release v$version exists ($RELEASES) and that this machine reaches github.com." \
                "Nothing was installed."
    done

    # The release's checksum for this one file. SHA256SUMS may list other files, which are not downloaded.
    want=
    sum=
    while read -r sum name || [ -n "$sum" ]; do
        case $name in
            "$asset" | "*$asset") want=$sum ;;
        esac
        sum=
    done <"$tmp/SHA256SUMS"
    case $want in
        *[!0-9a-f]* | '') want= ;;
    esac
    [ "${#want}" -eq 64 ] ||
        fail "the SHA256SUMS of release v$version has no checksum for $asset." "Nothing was installed."
    got=$(sha256sum <"$tmp/$asset")
    got=${got%% *}
    [ "$got" = "$want" ] ||
        fail "the checksum of the downloaded $asset is wrong." \
            "SHA256SUMS says $want" \
            "the file is    $got" \
            "Nothing was installed. Run this again; if it happens twice, do not use this download."

    chmod 0755 "$tmp/$asset"
    is=$("$tmp/$asset" --version </dev/null) ||
        fail "the downloaded program does not start on this machine (\`$asset --version\` failed)." \
            "Nothing was installed."
    mv -f "${tmp:?}/$asset" "${bin:?}"
    cleanup
    # This script's own note of what it put there: --uninstall removes the program only while it is still this file.
    mkdir -p "$note_dir"
    printf '%s  %s\n' "$got" "$bin" >"$note"

    if [ -z "$was" ]; then
        program_note="installed: $is"
    elif [ "$was" = "$is" ]; then
        program_note="installed again: $is"
    else
        program_note="replaced: $is (it was: $was)"
    fi
}

# ----------------------------------------------------------------------------------------------------------------------
# 3. The config and the token, only where there is no config.
# ----------------------------------------------------------------------------------------------------------------------

write_config() {
    door_port=
    door_note=
    door_moved=
    config_flag_note=
    if exists "$config"; then
        # A config this script would write letter for letter, with the door on one of its ports, with the owner's no,
        # or without the key, is its own from an earlier run. Any other is the user's: this script changes nothing
        # in it, says nothing about what it makes the node do, and starts no node for it.
        config_ours=0
        config_note="kept as it was: this script changes nothing in a config it did not write"
        config_has=$(cat "$config" 2>/dev/null) || config_has=
        door_has=
        if [ "$config_has" = "$(config_text '')" ]; then
            door_has=absent
        elif [ "$config_has" = "$(config_text null)" ]; then
            door_has=null
        else
            for door_try in $DOOR_PORTS; do
                if [ "$config_has" = "$(config_text "$door_try")" ]; then
                    door_has=$door_try
                    break
                fi
            done
        fi
        if [ -n "$door_has" ]; then
            config_ours=1
            config_note="kept as it was: the config this script writes, from an earlier run"
            settle_door "$door_has"
        elif [ -n "$door_flag" ]; then
            config_flag_note="--$door_flag changed nothing: this config is not one this script wrote. Its own word for no door is \"loopback\": null in \"listen\"."
        fi
        if exists "$token_file"; then
            token_note="kept as it was"
        else
            token_note="missing: this script mints no token for a config that exists"
        fi
        door_words
        return 0
    fi

    mkdir -p "$config_dir/tokens" "$state_dir"
    chmod 0700 "$config_dir/tokens" "$state_dir"

    # `serve` reads its tokens when it starts, so the token comes before the node. The config comes last: a run that
    # stops here leaves no config, and the next run starts this step again.
    if exists "$token_file"; then
        token_note="kept as it was"
    else
        "$bin" token mint --scope viewer --id "$TOKEN_ID" --hash-dir "$config_dir/tokens" --out "$token_file" \
            >/dev/null </dev/null ||
            fail "\`arca token mint\` failed; its own words are above." \
                "If it says a token with the id $TOKEN_ID exists, the token's file is gone and its hash is not:" \
                "remove $config_dir/tokens/$TOKEN_ID.hash and run this again."
        token_note="minted now, scope viewer"
    fi

    settle_door new
    config_ours=1
    config_note="written now: it names no kernel project, so the node only reads"
    door_words
}

# Decides the door of this script's own config and writes the config when the decision changes it. $1 is what the
# config says now: `new` (there is none yet), `absent` (it does not name the key), `null` (the owner's no) or a port.
#
#   --no-door              no door, written as null: the config itself says no, and no later run overrides it
#   --door                 a door, where there is none
#   neither, a new config  a door
#   neither, a port        kept
#   neither, null          kept: the owner said no
#   neither, absent        a door only when this run replaces a program from before the door with one that has it,
#                          and the owner has not said no; otherwise kept. A config without the key under a program
#                          that has the door is the owner's doing, or a run that found no free port: no run opens
#                          one there unasked.
#
# An older program refuses a config that names the key at all, so for one the key is left out, and a no is kept in
# this script's own note instead, for the run that installs a program that can hold it.
settle_door() {
    door_was=$1
    door_no=$note_dir/no-door
    case $door_flag in
        no-door) : >"$door_no" ;;
        door) rm -f "${door_no:?}" ;;
    esac
    door_want=$door_was
    if ! program_has_door; then
        # A no the owner wrote by hand has only the config to live in, and this program's config cannot hold it: it
        # goes into the note, as --no-door's does. A null is that no; so is a config without the key under a program
        # that had the door.
        if [ -z "$door_flag" ]; then
            if [ "$door_was" = null ] ||
                { [ "$door_was" = absent ] && [ -n "$was_version" ] && version_has_door "$was_version"; }; then
                : >"$door_no"
            fi
        fi
        door_want=absent
        door_note="none: this program is older than $DOOR_SINCE, the first release that has one"
    elif [ "$door_flag" = no-door ] || { [ -z "$door_flag" ] && exists "$door_no" && { [ "$door_was" = new ] || [ "$door_was" = absent ]; }; }; then
        door_want=null
    elif [ "$door_flag" = door ]; then
        case $door_was in
            new | absent | null) door_want=open ;;
        esac
    elif [ "$door_was" = new ]; then
        door_want=open
    elif [ "$door_was" = absent ] && [ -n "$was_version" ] && ! version_has_door "$was_version"; then
        door_want=open
    fi
    if [ "$door_want" = open ]; then
        if door_want=$(free_door_port); then
            door_moved=new
        else
            # Nothing is opened, and what the config said stays said.
            door_want=$door_was
            door_note="none: something listens on every port from ${DOOR_PORTS%% *} to ${DOOR_PORTS##* } on this machine"
        fi
    fi
    [ "$door_want" != new ] || door_want=absent

    if [ "$door_want" != "$door_was" ]; then
        case $door_want in
            absent) put_config '' ;;
            *) put_config "$door_want" ;;
        esac
        if [ "$door_was" != new ]; then
            case $door_want in
                absent) config_note="written again without the HTTP door's key: this program is older than $DOOR_SINCE and refuses a config that names it" ;;
                null) config_note="written again with no HTTP door (\"loopback\": null): --no-door asked for none" ;;
                *)
                    if [ "$door_flag" = door ]; then
                        config_note="written again with the HTTP door: --door asked for it"
                    else
                        config_note="written again: it was this script's own, from before the HTTP door; the door is all that was added"
                    fi
                    ;;
            esac
        fi
    fi
    case $door_want in
        absent)
            [ -n "$door_note" ] ||
                door_note="none: the config names no door, and this script opens none unasked in a config that has been without one. To open it: sh install.sh --door"
            ;;
        null) door_note="none: the config says so (\"loopback\": null). To open it: sh install.sh --door" ;;
        *) door_port=$door_want ;;
    esac
}

# What the report says about the door of a config that is this script's. A config that is not gets no line: this
# script says nothing about what such a config opens.
door_words() {
    door_url=
    [ -n "$door_port" ] || return 0
    door_url=http://127.0.0.1:$door_port/mcp
    door_note="$door_url, from this machine only"
    if [ "$door_moved" = new ] && [ "$door_port" != "${DOOR_PORTS%% *}" ]; then
        door_note="$door_note; port ${DOOR_PORTS%% *} was in use, so the next free one was taken"
    fi
}

# ----------------------------------------------------------------------------------------------------------------------
# 4. The service: only for a node this script can vouch for, and can ask afterwards.
# ----------------------------------------------------------------------------------------------------------------------

start_service() {
    # Without a token the node cannot be asked what it is, before or after, so nothing is started or restarted.
    if ! exists "$token_file"; then
        service=unasked
        service_note="nothing started: there is no token to ask the node with"
        return 0
    fi
    # A config this script did not write, and no service of this script's from an earlier run: the node is the
    # user's to start. It may be one that writes.
    if [ "$config_ours" = 0 ] && ! exists "$unit_file"; then
        if ask_once; then
            service=other
            service_note="none written: a node already answers for this config, and this script did not start it"
        else
            service=declined
            service_note="none written: this script starts no node for a config it did not write"
        fi
        return 0
    fi
    if ! user_systemd; then
        service=none
        service_note="none: \`systemctl --user\` does not answer in this session"
        return 0
    fi
    # A node that already answers for this config, started some other way (a terminal, a unit of the user's own),
    # holds the socket: a second one beside it could not start.
    if ! systemctl --user is-active --quiet "$UNIT" </dev/null && ask_once; then
        service=other
        service_note="none written: a node already answers for this config, and it is not this script's service"
        return 0
    fi

    mkdir -p "$unit_dir"
    tmp_file=$(mktemp "$unit_dir/.$UNIT.XXXXXX")
    cat >"$tmp_file" <<EOF
$UNIT_MARK A second run rewrites this file, and \`install.sh --uninstall\` removes it.
# It runs the node that $config describes.

[Unit]
Description=Automagik Arca node
StartLimitIntervalSec=600
StartLimitBurst=5

[Service]
Type=simple
ExecStart=$bin serve --config $config
Restart=on-failure
RestartSec=10
UMask=0077

[Install]
WantedBy=default.target
EOF
    chmod 0644 "$tmp_file"
    mv -f "${tmp_file:?}" "${unit_file:?}"
    tmp_file=
    systemctl --user daemon-reload </dev/null ||
        fail "\`systemctl --user daemon-reload\` failed after $unit_file was written."
    systemctl --user enable "$UNIT" </dev/null >/dev/null 2>&1 ||
        fail "\`systemctl --user enable $UNIT\` failed." "Look at: systemctl --user status $UNIT"
    # `restart` starts a unit that is not running, and puts a running node on the program just installed.
    systemctl --user restart "$UNIT" </dev/null ||
        fail "\`systemctl --user restart $UNIT\` failed." \
            "Look at: systemctl --user status $UNIT" \
            "and:     journalctl --user -u $UNIT -n 50"
    service=unit
    service_note="$UNIT, a systemd user unit ($unit_file): enabled and started"
}

# ----------------------------------------------------------------------------------------------------------------------
# 5. The node's own answer, and how to connect an assistant.
# ----------------------------------------------------------------------------------------------------------------------

# One question to the node. The answer is one line of JSON; the token is read from its file by `arca` itself.
ask_once() {
    status=
    [ -e "$token_file" ] || return 1
    status=$("$bin" call arca_status --config "$config" --token-file "$token_file" 2>/dev/null </dev/null)
}

# The same, once a second until the node this script just started answers. A node it did not start is asked once.
ask_status() {
    tries=1
    [ "$service" = unit ] && tries=$wait_seconds
    while :; do
        if ask_once; then
            return 0
        fi
        tries=$((tries - 1))
        [ "$tries" -gt 0 ] || return 1
        sleep 1
    done
}

# --enable-changes: the one step from a node that only looks to one that may change things, taken by the program
# itself. This script reads no standard input, so the flag is the owner's yes, and init takes its own defaults. It is
# run only for the node this script runs as a service, which init then restarts and asks; its words are the report's.
enable_changes() {
    init_said=
    init_done=
    [ "$enable_flag" = 1 ] || return 0
    init_command="$bin init --enable-changes --yes --config $config"
    if [ "$service" != unit ]; then
        init_done=skipped
        init_said="This script runs \`arca init\` only for the node it runs as a service, and it started none here."
        return 0
    fi
    # The program says whether it has the step: one from before it would only print its usage.
    case $("$bin" --help 2>/dev/null </dev/null) in
        *'init --enable-changes'*) ;;
        *)
            init_done=absent
            init_said="This program has no \`arca init\`: it is from a release before that step."
            return 0
            ;;
    esac
    if init_said=$("$bin" init --enable-changes --yes --config "$config" 2>&1 </dev/null); then
        init_done=yes
    else
        init_done=no
    fi
}

# The templates of the node this script runs as a service, on an upgrade. `arca init` installs a node's templates; a
# later release may change one, and a node must not keep a template that release fixed. The program does the work
# and decides: a file that is an earlier release's own, byte for byte, becomes this release's, and a file the owner
# edited is kept. This script only runs it and says what it answered.
#
# It runs only where templates are already installed, in the state directory this script names in the config it
# writes, and only while the config still names that directory: a node that only looks has none, and gets none here.
upgrade_templates() {
    templates_replaced=
    templates_kept=
    templates_failed=
    [ "$service" = unit ] || return 0
    exists "$state_dir/templates" || return 0
    case ${config_has-} in
        *"\"$state_dir\""*) ;;
        *) return 0 ;;
    esac
    templates_command="$bin templates install --config $config"
    if ! templates_said=$("$bin" templates install --config "$config" 2>/dev/null </dev/null); then
        templates_failed=1
        return 0
    fi
    # A program from before it named what it replaced answers without that list: there is then nothing to say.
    templates_replaced=$(json_list "$templates_said" replaced) || templates_replaced=
    templates_kept=$(json_list "$templates_said" kept) || templates_kept=
}

report() {
    status_command="$bin call arca_status --config $config --token-file $token_file"
    start_command="$bin serve --config $config"
    result=0
    answered=0
    reach=
    case $service in
        # A node this script did not start answered a moment ago, when the service was decided.
        other) answered=1 ;;
        unit | none)
            if ask_status; then
                answered=1
            fi
            ;;
    esac

    say
    case $service in
        unit | other) say "Automagik Arca is installed for the user $user." ;;
        *) say "The program of Automagik Arca is installed for the user $user. This script started no node." ;;
    esac
    say
    say "  program   $bin"
    say "            $program_note"
    say "  config    $config"
    say "            $config_note"
    if [ -n "$config_flag_note" ]; then
        say "            $config_flag_note"
    fi
    say "  token     $token_file"
    if exists "$token_file"; then
        say "            $token_note; readable by this user only, and never shown here"
    else
        say "            $token_note"
    fi
    if [ "$config_ours" = 1 ]; then
        say "  state     $state_dir"
    fi
    if [ -n "${templates_replaced-}" ]; then
        say "  templates brought up to this release: $templates_replaced"
        say "            (each was an earlier release's own file, not edited; a service that already runs is not changed)"
    fi
    if [ -n "${templates_kept-}" ]; then
        say "  templates kept as you edited them: $templates_kept"
        say "            To replace them with this release's, losing those edits:"
        say "              $templates_command --replace"
    fi
    if [ -n "${templates_failed-}" ]; then
        say "  templates NOT checked: \`arca templates install\` failed. Run it to see why:"
        say "              $templates_command"
    fi
    say "  service   $service_note"
    # The node says whether it holds the door's port. A port another program took first does not stop the node: it
    # runs without the door and keeps trying.
    door_shut=
    if [ "$answered" = 1 ] && [ -n "$door_url" ]; then
        case $status in
            *'"open":false'*) door_shut=$(json_text "$status" reason) || door_shut="the node does not say why" ;;
        esac
    fi
    if [ -n "$door_shut" ]; then
        say "  door      CLOSED: the node could not take $door_url"
        say "            ($door_shut). It runs without the door and keeps trying the port."
    elif [ -n "$door_note" ]; then
        say "  door      $door_note"
    fi
    if [ "$answered" = 1 ]; then
        mode=$(json_text "$status" mode) || mode="not stated"
        node_version=$(json_text "$status" version) || node_version="not stated"
        case $status in
            *'"incusReachable":true'*) reach=yes ;;
            *) reach=no ;;
        esac
        if [ "$reach" = yes ]; then
            say "  node      mode $mode, version $node_version, Incus answers"
        else
            say "  node      mode $mode, version $node_version, INCUS DOES NOT ANSWER THE NODE"
        fi
        if [ "$mode" != watch-only ]; then
            if [ "$init_done" = yes ]; then
                say "            This node may now make changes: --enable-changes asked for it. Its words above say what waits for you."
            else
                say "            This node is not read-only: its config, which this script did not write, says so."
            fi
        fi
    elif exists "$token_file"; then
        say "  node      it has not answered \`arca_status\`"
    else
        say "  node      not asked: there is no token to ask it with"
    fi
    if [ -n "${runtime_derived-}" ]; then
        say
        say "This shell had no XDG_RUNTIME_DIR, so $XDG_RUNTIME_DIR was used. Set it the same way before"
        say "\`systemctl --user\` in this shell: export XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR"
    fi
    case ":${PATH-}:" in
        *":$bin_dir:"*) ;;
        *)
            say
            say "$bin_dir is not on this shell's PATH. The commands below name the program by its full path."
            ;;
    esac

    if [ -n "$init_done" ]; then
        say
        case $init_done in
            yes)
                say "--enable-changes: \`arca init\` ran, and these are its words."
                ;;
            no)
                say "--enable-changes was NOT done: \`arca init\` stopped, and the node still only looks. Its words:"
                result=1
                ;;
            *)
                say "--enable-changes was NOT done, and the node still only looks."
                result=1
                ;;
        esac
        say
        printf '%s\n' "$init_said" | while IFS= read -r init_line; do
            say "    $init_line"
        done
        if [ "$init_done" != yes ]; then
            say
            say "  To take the step yourself once that is settled:"
            say "    $init_command"
        fi
    fi
    say
    case $service in
        unit)
            if [ "$answered" = 0 ]; then
                say "The service was started and the node did not answer within $wait_seconds seconds. Look at:"
                say "  systemctl --user status $UNIT"
                say "  journalctl --user -u $UNIT -n 50"
                say "Then ask it again:"
                say "  $status_command"
                return 1
            fi
            if [ ! -e "/var/lib/systemd/linger/$user" ]; then
                say "The service runs while $user has a session on this machine, and starts again at the next login."
                say "To keep it running without a session, the machine's administrator turns lingering on for $user"
                say "(\`loginctl enable-linger $user\`)."
                say
            fi
            ;;
        other)
            say "The program was replaced, and the node that is running still uses the one it started with."
            say "This script started nothing and stopped nothing."
            if [ "$config_ours" = 1 ]; then
                say "To run the node as a service on the new program, stop that node and run this script again."
            fi
            say
            ;;
        none)
            if [ "$answered" = 0 ]; then
                say "The node is not running: this session has no \`systemctl --user\` to run it as a service."
                say "Start it yourself with this one command; it runs until it is stopped:"
                say "  $start_command"
                say
                result=$NOT_STARTED
            fi
            ;;
        declined)
            say "No node was started: this script did not write $config, so it"
            say "cannot say what that config makes the node do, and it sets up no service for it."
            say "If the node is yours to start, this one command starts it; it runs until it is stopped:"
            say "  $start_command"
            return "$NOT_STARTED"
            ;;
        unasked)
            say "No node was started or restarted: there is no token at $token_file,"
            say "so this script cannot ask the node what it is, and it starts no node it cannot ask."
            say "A node that is running still uses the program it started with."
            say "If the node is yours to start, this one command starts it; it runs until it is stopped:"
            say "  $start_command"
            say "To ask it, mint a token first (the node reads its tokens when it starts):"
            say "  $bin token mint --scope viewer --id <id> --config $config --out <file>"
            say "  $bin call arca_status --config $config --token-file <file>"
            return "$NOT_STARTED"
            ;;
    esac

    if [ "$reach" = no ]; then
        say "The node answers, and Incus does not answer the node: it can read nothing. Its config names the Incus"
        say "socket it uses ($config). Ask it again once Incus answers there:"
        say "  $status_command"
        return 1
    fi

    if [ -n "$door_shut" ]; then
        # Another program holds the door's port. Whatever answers there is not this node, so nothing is printed that
        # would send the token to it: no line to run, and no entry to paste.
        say "Connect an assistant. One way works right now:"
        say
        say "  1. An assistant with a shell on this machine, as the user $user, runs \`arca call <verb>\`:"
        say "       $status_command"
        say "     \`watch_report\` in place of \`arca_status\` lists every project and instance with its owner."
        say
        say "  The HTTP door is CLOSED: the node could not take port $door_port ($door_shut)."
        say "  Whatever answers on that port now is not this node, so this script prints no line and no entry for it:"
        say "  a token sent there would go to that program. The node keeps trying the port. The command above says"
        say "  \"open\":true under \"loopback\" once it has it; run this script again then, and it prints the line."
        say "  To have no door at all: sh install.sh --no-door"
        return "$result"
    fi
    say "Connect an assistant. Two ways work today:"
    say
    if [ -n "$door_url" ]; then
        # The report shows what connects an assistant and one line that says where the cautions are. They are the
        # program's to say, for this node, when a person asks: a program from before that names the guide instead.
        case $("$bin" --help 2>/dev/null </dev/null) in
            *'--security'*) door_cautions="$bin status --security --config $config" ;;
            *) door_cautions="$GUIDE#loopback-listener" ;;
        esac
        say "  1. An assistant that takes an MCP server URL. On a machine other people log in to, paste the entry below"
        say "     instead of running the line, and read this first: $door_cautions"
        say "     For Claude Code, run this one line; the shell reads the token from its file, so it is not shown here:"
        say "       claude mcp add --transport http arca $door_url --header \"Authorization: Bearer \$(cat '$token_file')\""
        say "     Any other client takes this entry, with the content of $token_file"
        say "     in place of <token>:"
        say "       { \"mcpServers\": { \"arca\": { \"type\": \"http\", \"url\": \"$door_url\", \"headers\": { \"Authorization\": \"Bearer <token>\" } } } }"
        if [ "$service" != unit ]; then
            say "     This script started no node: the door is open once a node has started with this config."
        fi
        say
        say "  2. An assistant with a shell on this machine, as the user $user, runs \`arca call <verb>\`:"
        say "       $status_command"
        say "     \`watch_report\` in place of \`arca_status\` lists every project and instance with its owner."
        return "$result"
    fi
    say "  1. An assistant with a shell on this machine, as the user $user, runs \`arca call <verb>\`:"
    say "       $status_command"
    say "     \`watch_report\` in place of \`arca_status\` lists every project and instance with its owner."
    say
    say "  2. An assistant that takes an MCP server URL needs a TCP address. The node opens one only when its"
    say "     config names \`listen.loopback\`, \`listen.tailnet\` or \`listen.proxy\`, and this script wrote none of"
    say "     them here. The operating guide says how to turn one on: $GUIDE#the-config-file"
    say "     The client's entry is then the URL http://<address>:<port>/mcp and the header"
    say "     \"Authorization: Bearer <the content of $token_file>\"."
    return "$result"
}

# ----------------------------------------------------------------------------------------------------------------------
# 6. --uninstall: the program and the unit go; the config, the tokens and the state stay.
# ----------------------------------------------------------------------------------------------------------------------

uninstall() {
    find_runtime_dir || true

    if ! exists "$unit_file"; then
        say "service   there is no $unit_file; nothing to remove"
    elif ! unit_is_ours; then
        say "service   $unit_file was not written by this script; it is left alone"
    elif user_systemd; then
        systemctl --user disable --now "$UNIT" </dev/null >/dev/null 2>&1 ||
            fail "\`systemctl --user disable --now $UNIT\` failed; nothing was removed." \
                "Look at: systemctl --user status $UNIT"
        rm -f "${unit_file:?}"
        systemctl --user daemon-reload </dev/null || true
        say "service   $UNIT was stopped and disabled, and $unit_file was removed"
    else
        # With no user manager to ask, what `enable` left is removed by hand: the unit, and the link that starts it.
        rm -f "${unit_file:?}"
        if [ -L "$unit_wanted" ]; then
            rm -f "${unit_wanted:?}"
        fi
        say "service   $unit_file was removed, with the link that starts it at login."
        say "          \`systemctl --user\` does not answer in this session, so a node that is running was not"
        say "          stopped; it ends with this user's session"
    fi

    # The program goes only while it is still the file this script put there.
    noted=
    if [ -f "$note" ]; then
        read -r noted _rest <"$note" || true
    fi
    if ! exists "$bin"; then
        say "program   there is no $bin; nothing to remove"
    elif guide_name=$(guide_unit); then
        say "program   $bin is left in place: $guide_name, the operating guide's unit, runs it"
    elif [ -z "$noted" ]; then
        say "program   $bin is left in place: this script has no note of having installed it"
    elif ! have sha256sum; then
        say "program   $bin is left in place: without \`sha256sum\` this script cannot tell that it is"
        say "          still the file it installed"
    else
        now=$(sha256sum <"$bin") || now=
        now=${now%% *}
        if [ "$now" = "$noted" ]; then
            rm -f "${bin:?}"
            say "program   $bin was removed"
        else
            say "program   $bin is left in place: it is no longer the file this script installed"
        fi
    fi
    if ! exists "$bin" && exists "$note_dir"; then
        rm -rf "${note_dir:?}"
    fi

    say
    say "Kept, because they are yours:"
    say "  config   $config"
    say "  tokens   $config_dir/tokens and $token_file"
    say "  state    $state_dir"
    say "Run the installer again and the node comes back with the same config and the same token."
}

main() {
    # What this script makes is this user's alone, unless it sets a mode itself.
    umask 077

    version=$ARCA_VERSION
    version_named=0
    action=install
    door_flag=
    enable_flag=0
    while [ "$#" -gt 0 ]; do
        case $1 in
            --version)
                [ "$#" -ge 2 ] || fail "--version needs a value, such as --version $ARCA_VERSION"
                version=$2
                version_named=1
                shift
                ;;
            --version=*)
                version=${1#--version=}
                version_named=1
                ;;
            --uninstall) action=uninstall ;;
            --enable-changes) enable_flag=1 ;;
            --no-door)
                [ "$door_flag" != door ] || fail "--door and --no-door are not both given."
                door_flag=no-door
                ;;
            --door)
                [ "$door_flag" != no-door ] || fail "--door and --no-door are not both given."
                door_flag=door
                ;;
            -h | --help)
                usage
                exit 0
                ;;
            *)
                usage >&2
                fail "unknown argument: $1"
                ;;
        esac
        shift
    done
    version=${version#v}
    case $version in
        '' | *[!0-9A-Za-z.+-]*)
            fail "\"$version\" is not a release version. A version looks like $ARCA_VERSION; see $RELEASES"
            ;;
    esac

    [ -n "${HOME-}" ] || fail "HOME is not set, so there is nowhere to install." \
        "Run this as the user who will own the node."
    bin_dir=$HOME/.local/bin
    bin=$bin_dir/arca
    config_dir=$HOME/.config/arca
    config=$config_dir/node.json
    token_file=$config_dir/$TOKEN_ID.secret
    state_dir=$HOME/.local/state/arca-node
    note_dir=$HOME/.local/state/arca-install
    note=$note_dir/program.sha256
    unit_dir=$HOME/.config/systemd/user
    unit_file=$unit_dir/$UNIT
    unit_wanted=$unit_dir/default.target.wants/$UNIT
    incus_socket=${INCUS_SOCKET:-/var/lib/incus/unix.socket}
    tcp_tables=${ARCA_INSTALL_TCP_TABLES:-/proc/net/tcp /proc/net/tcp6}
    node_socket=
    wait_seconds=${ARCA_INSTALL_WAIT:-30}
    case $wait_seconds in
        '' | *[!0-9]*) fail "ARCA_INSTALL_WAIT is a whole number of seconds, not \"$wait_seconds\"." ;;
    esac
    uid=$(id -u 2>/dev/null) || uid=
    user=$(id -un 2>/dev/null) || user=${USER:-this user}

    trap cleanup EXIT
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM

    if [ "$action" = uninstall ]; then
        plain_path "$HOME" || fail "the home directory \"$HOME\" is not a plain absolute path."
        uninstall
        exit 0
    fi

    check_machine
    check_user
    check_runtime_dir
    check_incus
    check_units
    check_installed
    if [ "$problems" -gt 0 ]; then
        printf 'install: nothing was changed. Fix what is listed above, then run this again.\n' >&2
        exit 1
    fi

    if exists "$config"; then
        say "Installing Automagik Arca $version for the user $user, with the config that is at $config."
    else
        say "Installing Automagik Arca $version as a read-only node for the user $user."
    fi
    say "Checked: Linux on x86_64, glibc $glibc, and Incus answers on $incus_socket."
    install_program
    write_config
    start_service
    upgrade_templates
    enable_changes
    report
}

# The whole script is read before anything runs, so a download that was cut short does nothing.
main "$@"

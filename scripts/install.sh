#!/bin/sh
# Install Automagik Arca as a read-only node for the user who runs this, in one command.
#
#   sh install.sh                      install, or upgrade in place, the release named below
#   sh install.sh --version <version>  the same, for another release
#   sh install.sh --uninstall          remove the program and the service this script installed
#   sh install.sh --help
#
# It leaves what the README's "Quickstart: a read-only node in five minutes" leaves:
#
#   ~/.local/bin/arca                        the program, checked against the release's SHA256SUMS
#   ~/.config/arca/node.json                 the quickstart's smallest config; it names no kernel project, so the node
#                                            only reads: it sends Incus nothing but GET
#   ~/.config/arca/tokens/                   mode 0700; holds the hash of the one token
#   ~/.config/arca/assistant-1.secret        one `viewer` token, readable by this user only; never printed
#   ~/.local/state/arca-node/                mode 0700; the node's state
#   ~/.config/systemd/user/arca.service      where `systemctl --user` answers: the node as a service, enabled, started
#
# What it never does: it never runs sudo or su, never writes outside this user's home and runtime directory
# (XDG_RUNTIME_DIR), never changes a config that exists, never prints the token, never reads its standard input (so
# `curl ... | sh` works), and never repairs the machine: when something is missing it says what and how to fix it,
# and stops before it has changed anything.
#
# A second run downloads the release again, replaces the program in place and restarts the service.
#
# Three values come from the environment: INCUS_SOCKET, the Incus socket when it is not /var/lib/incus/unix.socket
# (the name the Incus client itself reads); ARCA_INSTALL_WAIT, how many seconds to wait for the started node's first
# answer (30); and ARCA_INSTALL_RUN_ROOT, where per-user runtime directories live (/run/user), for the tests.
set -eu

# The release this script installs. `--version` names another. This is not "the latest release": GitHub's address for
# that skips pre-releases and answers 404 while every release of Arca is one.
ARCA_VERSION=0.1.0-alpha.7

RELEASES=https://github.com/automagik-si/arca/releases
GUIDE=https://github.com/automagik-si/arca/blob/dev/docs/OPERATING.md
UNIT=arca.service
UNIT_MARK='# Automagik Arca: written by scripts/install.sh.'
# The two units the operating guide installs by hand. A user who has them already runs the node as a service.
GUIDE_UNITS='arca-watch.service arca-shim.service'
TOKEN_ID=assistant-1
# The oldest glibc the published program starts on.
GLIBC_MAJOR=2
GLIBC_MINOR=34
# Every program this script starts, but for `arca` itself and `systemctl`, which is used only where it answers.
INSTALL_PROGRAMS='uname id getconf curl sha256sum mktemp mkdir chmod mv rm cat ls sleep'

tmp=
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
EOF
}

cleanup() {
    if [ -n "$tmp" ]; then
        rm -rf "${tmp:?}"
        tmp=
    fi
}

have() {
    command -v "$1" >/dev/null 2>&1
}

# Whether anything has this name, a link that points nowhere included.
exists() {
    [ -e "$1" ] || [ -L "$1" ]
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

# The string value of the first "key":"value" in one line of compact JSON.
json_text() {
    case $1 in
        *"\"$2\":\""*) ;;
        *) return 1 ;;
    esac
    json_rest=${1#*"\"$2\":\""}
    printf '%s\n' "${json_rest%%\"*}"
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
    have id || return 1
    runtime_guess="${ARCA_INSTALL_RUN_ROOT:-/run/user}/$(id -u)"
    [ -d "$runtime_guess" ] || return 1
    XDG_RUNTIME_DIR=$runtime_guess
    export XDG_RUNTIME_DIR
    runtime_derived=1
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
            "Run this on an x86_64 machine, or build Arca from source (README, \"Build and test\")."
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

check_home() {
    if ! plain_path "$HOME"; then
        problem "the home directory \"$HOME\" is not a path this script can write into the config." \
            "It must be absolute and hold no space, quote, backslash, dollar, percent or backquote."
    elif [ ! -d "$HOME" ] || [ ! -w "$HOME" ]; then
        problem "the home directory $HOME is not a directory this user can write." \
            "Run this as the user who will own the node, with HOME set to that user's home."
    fi
}

check_runtime_dir() {
    if ! find_runtime_dir; then
        if [ -n "${XDG_RUNTIME_DIR-}" ]; then
            problem "XDG_RUNTIME_DIR is $XDG_RUNTIME_DIR, which is not a directory." \
                "The node's socket lives there. Unset it, or set it to this user's runtime directory, then run this again."
        else
            problem "XDG_RUNTIME_DIR is not set and ${ARCA_INSTALL_RUN_ROOT:-/run/user}/$(id -u 2>/dev/null || echo '<uid>') does not exist." \
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
            "Upgrade that install by hand instead: step 1 of the README's quickstart, then restart the service."
    elif exists "$unit_file" && ! unit_is_ours; then
        problem "$unit_file exists and this script did not write it." \
            "The installer's service has that name. Move that file away or rename your unit, then run this again."
    fi
}

# ----------------------------------------------------------------------------------------------------------------------
# 2. The program: one named release, checked against its SHA256SUMS, then put in place in one step.
# ----------------------------------------------------------------------------------------------------------------------

install_program() {
    asset="arca-$version-linux-x86_64"
    base="$RELEASES/download/v$version"

    was=
    if [ -x "$bin" ]; then
        was=$("$bin" --version 2>/dev/null </dev/null) || was=
    fi

    mkdir -p "$bin_dir"
    # The download lands beside its destination: inside the home, and on the same file system, so the last step is a
    # rename and a running node keeps the program it started with.
    tmp=$(mktemp -d "$bin_dir/.arca-install.XXXXXX")

    say "Downloading $asset and SHA256SUMS from $base"
    for file in "$asset" SHA256SUMS; do
        curl -fsSL --retry 2 --proto '=https' --tlsv1.2 -o "$tmp/$file" "$base/$file" </dev/null ||
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
    if exists "$config"; then
        config_note="kept as it was: this script changes nothing in a config that exists"
        if exists "$token_file"; then
            token_note="kept as it was"
        else
            token_note="missing: this script mints no token for a config it did not write"
        fi
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

    cat >"$config.new" <<EOF
{
  "stateDir": "$state_dir",
  "incus": { "socket": "$incus_socket" },
  "listen": { "unix": "$node_socket" },
  "tokens": { "hashDir": "$config_dir/tokens" },
  "kernel": { "droiddProjects": [] }
}
EOF
    mv -f "${config:?}.new" "${config:?}"
    config_note="written now: it names no kernel project, so the node only reads"
}

# ----------------------------------------------------------------------------------------------------------------------
# 4. The service.
# ----------------------------------------------------------------------------------------------------------------------

start_service() {
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
    cat >"$unit_file.new" <<EOF
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
    chmod 0644 "$unit_file.new"
    mv -f "${unit_file:?}.new" "${unit_file:?}"
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

# The same, once a second until the node this script just started answers.
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

report() {
    status_command="$bin call arca_status --config $config --token-file $token_file"
    answered=0
    if ask_status; then
        answered=1
    fi

    say
    say "Automagik Arca is installed for the user $user."
    say
    say "  program   $bin"
    say "            $program_note"
    say "  config    $config"
    say "            $config_note"
    say "  token     $token_file"
    say "            $token_note; readable by this user only, and never shown here"
    say "  state     $state_dir"
    say "  service   $service_note"
    if [ "$answered" = 1 ]; then
        mode=$(json_text "$status" mode) || mode="not stated"
        node_version=$(json_text "$status" version) || node_version="not stated"
        case $status in
            *'"incusReachable":true'*) reach="Incus answers" ;;
            *) reach="INCUS DOES NOT ANSWER THE NODE" ;;
        esac
        say "  node      mode $mode, version $node_version, $reach"
        if [ "$mode" != watch-only ]; then
            say "            This node is not read-only: its config, which this script did not change, says so."
        fi
    else
        say "  node      it has not answered \`arca_status\`"
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

    say
    case $service in
        unit)
            if [ "$answered" = 0 ] && exists "$token_file"; then
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
            say "To run the node as a service on the new program, stop that node and run this script again."
            say
            ;;
        none)
            if [ "$answered" = 0 ]; then
                say "The node is not running: this session has no \`systemctl --user\` to run it as a service."
                say "Start it yourself with this one command; it runs until it is stopped:"
                say "  $bin serve --config $config"
                say
            fi
            ;;
    esac

    if ! exists "$token_file"; then
        say "There is no token at $token_file, so the node was not asked for its status. Mint one, restart"
        say "the node (it reads its tokens when it starts), then ask:"
        say "  $bin token mint --scope viewer --id <id> --config $config --out <file>"
        say "  $bin call arca_status --config $config --token-file <file>"
        return 0
    fi

    say "Connect an assistant. Two ways work today:"
    say
    say "  1. An assistant with a shell on this machine, as the user $user, runs \`arca call <verb>\`:"
    say "       $status_command"
    say "     \`watch_report\` in place of \`arca_status\` lists every project and instance with its owner."
    say
    say "  2. An assistant that takes an MCP server URL needs a TCP address. The node opens one only when its"
    say "     config names \`listen.tailnet\` or \`listen.proxy\`, and the config this script writes names neither."
    say "     The operating guide says how to turn one on: $GUIDE#the-config-file"
    say "     The client's entry is then the URL http://<address>:<port>/mcp and the header"
    say "     \"Authorization: Bearer <the content of $token_file>\"."
    return 0
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
    else
        if user_systemd; then
            systemctl --user disable --now "$UNIT" </dev/null >/dev/null 2>&1 ||
                fail "\`systemctl --user disable --now $UNIT\` failed; nothing was removed." \
                    "Look at: systemctl --user status $UNIT"
            rm -f "${unit_file:?}"
            systemctl --user daemon-reload </dev/null || true
            say "service   $UNIT was stopped and disabled, and $unit_file was removed"
        else
            rm -f "${unit_file:?}"
            say "service   $unit_file was removed. \`systemctl --user\` does not answer in this session, so a"
            say "          node that is running was not stopped; it ends with this user's session"
        fi
    fi

    if guide_name=$(guide_unit); then
        say "program   $bin is left in place: $guide_name, the operating guide's unit, runs it"
    elif exists "$bin"; then
        rm -f "${bin:?}"
        say "program   $bin was removed"
    else
        say "program   there is no $bin; nothing to remove"
    fi

    say
    say "Kept, because they are yours:"
    say "  config   $config"
    say "  tokens   $config_dir/tokens and $token_file"
    say "  state    $state_dir"
    say "Run the installer again and the node comes back with the same config and the same token."
}

main() {
    version=$ARCA_VERSION
    action=install
    while [ "$#" -gt 0 ]; do
        case $1 in
            --version)
                [ "$#" -ge 2 ] || fail "--version needs a value, such as --version $ARCA_VERSION"
                version=$2
                shift
                ;;
            --version=*) version=${1#--version=} ;;
            --uninstall) action=uninstall ;;
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
    unit_dir=$HOME/.config/systemd/user
    unit_file=$unit_dir/$UNIT
    incus_socket=${INCUS_SOCKET:-/var/lib/incus/unix.socket}
    wait_seconds=${ARCA_INSTALL_WAIT:-30}
    case $wait_seconds in
        '' | *[!0-9]*) fail "ARCA_INSTALL_WAIT is a whole number of seconds, not \"$wait_seconds\"." ;;
    esac
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
    check_home
    check_runtime_dir
    check_incus
    check_units
    if [ "$problems" -gt 0 ]; then
        printf 'install: nothing was changed. Fix what is listed above, then run this again.\n' >&2
        exit 1
    fi

    say "Installing Automagik Arca $version as a read-only node for the user $user."
    say "Checked: Linux on x86_64, glibc $glibc, and Incus answers on $incus_socket."
    install_program
    write_config
    start_service
    report
}

# The whole script is read before anything runs, so a download that was cut short does nothing.
main "$@"

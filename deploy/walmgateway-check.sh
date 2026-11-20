#!/bin/sh
# walmgateway-check: keep the dashboard at http(s)://<controller>/walmgateway working.
#
# Runs ON THE CONTROLLER (busybox sh), at every start of the vwb-gateway container,
# before the gateway starts (deploy/entrypoint.sh, through nsenter into the controller's
# namespaces; the container runs with --pid host). Same approach as the kbus-gateway.
#
# Wanted config:    $WANT, written by the entrypoint just before (port from gateway.toml)
# Installed config: /etc/lighttpd/apps.confd/walmgateway.conf when the firmware has apps.confd
#                   (newer firmware), else /etc/lighttpd/fastcgi.confd/walmgateway.conf
#                   (firmware 04.04: lighttpd.conf includes fastcgi.confd/*.conf)
# A firmware update replaces /etc/lighttpd; Docker (in /home) and the container stay,
# so the next container start installs the config again.
#
# Missing or different -> install, check the whole lighttpd config with lighttpd -tt,
# restart lighttpd. A rejected config restores the old state, so the WBM keeps working.
# Exit status: 0 = in order (unchanged or installed), 1 = rejected or not possible.
WANT=${WANT:-/tmp/walmgateway-lighttpd.want}
DIR=/etc/lighttpd
CHECK_LOG=/tmp/walmgateway-lighttpd-check.log

log() {
    echo "walmgateway: $*"
    logger -t walmgateway "$*" 2>/dev/null || true
}

[ -f "$WANT" ] || { log "no wanted config: nothing to do"; exit 0; }
if ! command -v lighttpd >/dev/null 2>&1 || [ ! -f "$DIR/lighttpd.conf" ]; then
    log "no lighttpd on this controller: /walmgateway not available (dashboard on its own port)"
    exit 1
fi
if grep -q 'apps.confd' "$DIR/lighttpd.conf" && [ -d "$DIR/apps.confd" ]; then
    CONF=$DIR/apps.confd/walmgateway.conf
    OTHER=$DIR/fastcgi.confd/walmgateway.conf
elif grep -q 'fastcgi.confd' "$DIR/lighttpd.conf"; then
    CONF=$DIR/fastcgi.confd/walmgateway.conf
    OTHER=$DIR/apps.confd/walmgateway.conf
    mkdir -p "$DIR/fastcgi.confd"
else
    log "lighttpd.conf includes neither apps.confd nor fastcgi.confd: /walmgateway not available"
    exit 1
fi
# the config uses variables of the firmware's lighttpd.conf
for v in default_sockets response_header_policy_common mod_proxy; do
    grep -q "$v" "$DIR/lighttpd.conf" || { log "lighttpd.conf has no $v: /walmgateway not installed"; exit 1; }
done

if [ -f "$CONF" ] && cmp -s "$WANT" "$CONF" && [ ! -e "$OTHER" ]; then
    rm -f "$WANT"
    exit 0                                              # in order: nothing to do
fi

if [ -e "$CONF" ]; then
    what="updated"
    cp "$CONF" "$CONF.old"                              # *.old / *.new: not matched by *.conf
else
    what="installed (missing, e.g. after a firmware update)"
    rm -f "$CONF.old"
fi
[ -e "$OTHER" ] && mv "$OTHER" "$OTHER.old"            # never both (duplicate url condition)
cp "$WANT" "$CONF.new" && mv "$CONF.new" "$CONF"
rm -f "$WANT"

if ! (cd "$DIR" && lighttpd -tt -f lighttpd.conf) >"$CHECK_LOG" 2>&1; then
    if [ -e "$CONF.old" ]; then mv "$CONF.old" "$CONF"; else rm -f "$CONF"; fi
    [ -e "$OTHER.old" ] && mv "$OTHER.old" "$OTHER"
    log "lighttpd rejects $CONF, old state restored: $(grep -v 'X509 certificate' "$CHECK_LOG" | head -n 3 | tr '\n' ' ')"
    exit 1
fi
rm -f "$CONF.old" "$OTHER.old"

# Join the control groups of process $1. Started from the container (nsenter), this shell
# belongs to the container's cgroups; a lighttpd started from here would too, and docker
# would kill it with the next container stop. So the restart runs in the cgroups of the
# running lighttpd (cgroup v1 and v2).
join_cgroups() {
    [ -r "/proc/$1/cgroup" ] || return 0
    while IFS=: read -r _ ctrl path; do
        if [ -z "$ctrl" ]; then dir=/sys/fs/cgroup; else dir=/sys/fs/cgroup/${ctrl#name=}; fi
        [ -w "$dir$path/cgroup.procs" ] && echo $$ >"$dir$path/cgroup.procs" 2>/dev/null
    done <"/proc/$1/cgroup"
    return 0
}

pid=$(pidof lighttpd 2>/dev/null | cut -d' ' -f1)
if [ -n "$pid" ]; then
    join_cgroups "$pid"
    if /etc/init.d/lighttpd restart >/dev/null 2>&1; then
        log "$CONF $what, lighttpd restarted: /walmgateway available"
        pid=$(pidof lighttpd 2>/dev/null | cut -d' ' -f1)
        if [ -n "$pid" ] && grep -q docker "/proc/$pid/cgroup" 2>/dev/null; then
            log "WARNING: lighttpd runs in a docker cgroup; restart it on the controller: /etc/init.d/lighttpd restart"
        fi
    else
        log "$CONF $what, but the lighttpd restart failed: /etc/init.d/lighttpd restart"
    fi
else
    log "$CONF $what; lighttpd is not running yet and loads it when it starts"
fi
exit 0

#!/bin/sh
# Container start: make sure http(s)://<controller>/walmgateway works, then start the gateway.
#
# The dashboard check (deploy/walmgateway-check.sh) runs in the controller's namespaces
# through nsenter, so the container needs --pid host and the capabilities to enter them
# (deploy/deploy.sh: --pid host --cap-add SYS_ADMIN --cap-add SYS_PTRACE --cap-add SYS_CHROOT).
# Without them it is skipped and the dashboard stays reachable on its own port.
# WALMGW_DASHBOARD_CHECK=0 switches it off. It never stops the gateway from starting.
[ $# -gt 0 ] || set -- /config/gateway.toml
CFG=$1
SHARE=/usr/share/walmgateway

dashboard_check() {
    # dashboard port from [dashboard] bind in gateway.toml; no section = no dashboard
    port=$(sed -n '/^\[dashboard\]/,/^\[/{s/^bind *= *"[^"]*:\([0-9]*\)".*/\1/p}' "$CFG" 2>/dev/null | head -n 1)
    if [ -z "$port" ]; then
        echo "[walmgateway] no [dashboard] bind in $CFG: dashboard check skipped"
        return 0
    fi
    # with --pid host, PID 1 is the controller's init, not this script or docker-init
    if [ "$$" -eq 1 ] || tr '\0' ' ' </proc/1/cmdline 2>/dev/null | grep -q docker-init; then
        echo "[walmgateway] dashboard check skipped (container without --pid host): http://<host>:$port/ only"
        return 0
    fi
    # the wanted config travels inside the script (here-document), then the check runs on the host
    {
        echo "cat > /tmp/walmgateway-lighttpd.want <<'__WALMGW_EOF__'"
        sed "s/__PORT__/$port/" "$SHARE/walmgateway-lighttpd.conf"
        echo "__WALMGW_EOF__"
        cat "$SHARE/walmgateway-check.sh"
    } | nsenter -t 1 -m -u -i -n -p sh -s 2>&1 | sed 's/^/[dashboard] /'   # nsenter errors are shown too
}

[ "${WALMGW_DASHBOARD_CHECK:-1}" = 1 ] && dashboard_check

exec /vwb-gateway "$@"

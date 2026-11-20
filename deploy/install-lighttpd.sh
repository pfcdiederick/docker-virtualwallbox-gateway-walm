#!/bin/sh
# Make the gateway dashboard available at http(s)://<controller>/walmgateway/
#
#   deploy/install-lighttpd.sh <controller-ip> [dashboard-port]     (default port 3090)
#   deploy/install-lighttpd.sh <controller-ip> --remove
#
# Installs deploy/walmgateway-lighttpd.conf as /etc/lighttpd/fastcgi.confd/walmgateway.conf,
# checks the whole lighttpd config with lighttpd -tt and restarts lighttpd. A rejected
# config is removed again, so the WBM keeps working.
# Note: a firmware update replaces /etc/lighttpd; run this script again afterwards.
set -eu
PFC=${1:?usage: $0 <controller-ip> [port|--remove]}
ARG=${2:-3090}
SSH="ssh -o BatchMode=yes root@$PFC"
CONF=/etc/lighttpd/fastcgi.confd/walmgateway.conf

if [ "$ARG" = "--remove" ]; then
    $SSH "rm -f $CONF && /etc/init.d/lighttpd restart >/dev/null 2>&1 && echo 'removed $CONF, lighttpd restarted'"
    exit 0
fi

sed "s/__PORT__/$ARG/" "$(dirname "$0")/walmgateway-lighttpd.conf" | $SSH "cat > $CONF.new"
$SSH "set -e
    [ -e $CONF ] && cp $CONF $CONF.old || rm -f $CONF.old
    mv $CONF.new $CONF
    if ! (cd /etc/lighttpd && lighttpd -tt -f lighttpd.conf) >/tmp/walmgw-lighttpd.log 2>&1; then
        if [ -e $CONF.old ]; then mv $CONF.old $CONF; else rm -f $CONF; fi
        echo 'lighttpd rejects the config, old state restored:'; head -n 5 /tmp/walmgw-lighttpd.log; exit 1
    fi
    rm -f $CONF.old
    /etc/init.d/lighttpd restart >/dev/null 2>&1
    echo 'installed $CONF -> 127.0.0.1:$ARG, lighttpd restarted'"
echo "dashboard: http://$PFC/walmgateway/"

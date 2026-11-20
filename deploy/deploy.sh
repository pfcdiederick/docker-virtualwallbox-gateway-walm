#!/usr/bin/env bash
# Deploy the WALM gateway to a controller in one go (run on the build PC, from anywhere):
#
#   deploy/deploy.sh <controller-ip>              build, copy, start, set up /walmgateway
#   deploy/deploy.sh <controller-ip> --no-build   use the local image vwb-gateway:<VERSION>
#   deploy/deploy.sh <controller-ip> --rollback   back to the previous container
#
# Steps:
#   1. build vwb-gateway:<VERSION> for linux/arm/v7 (VERSION file), copy it to the controller
#   2. config folder ($VWB_DIR/config, default /root/vwb/config): created from config/ in this
#      project when missing; an existing one is NEVER overwritten (profile, login stay)
#   3. replace the container; the old one is kept stopped as vwb-gateway-prev. When the new one
#      does not keep running, the old one is started again automatically
#   4. the container itself sets up http(s)://<controller>/walmgateway/ at every start
#      (deploy/entrypoint.sh + walmgateway-check.sh: lighttpd -tt, a rejected config is removed
#      again), so it also comes back by itself after a firmware update
#   5. check http://<controller>/walmgateway/api/status, remove unused older images
#
# Needs: ssh root@<controller> with a key (ssh-copy-id root@<controller>), docker buildx.
set -euo pipefail
cd "$(dirname "$0")/.."
PFC=${1:?usage: $0 <controller-ip> [--no-build|--rollback]}
MODE=${2:-}
VERSION=$(tr -d ' \n' < VERSION)
IMAGE=vwb-gateway:$VERSION
VWB_DIR=${VWB_DIR:-/root/vwb}
SSH="ssh -o BatchMode=yes -o ConnectTimeout=5 root@$PFC"
# --pid host + capabilities: the start script sets up /walmgateway in the controller's lighttpd
# (nsenter into the controller's namespaces) at every start, e.g. again after a firmware update
RUN_OPTS="--restart unless-stopped --network host --pid host --cap-add SYS_ADMIN --cap-add SYS_PTRACE --cap-add SYS_CHROOT
          --memory 32m --cpus 0.3 --log-opt max-size=1m --log-opt max-file=2"
RUN_OPTS=$(echo $RUN_OPTS)

say() { echo ">>> $*"; }
$SSH true || { echo "no ssh access to root@$PFC (ssh-copy-id root@$PFC)"; exit 1; }

if [[ $MODE == --rollback ]]; then
    $SSH 'set -e; docker inspect vwb-gateway-prev >/dev/null 2>&1 || { echo "no vwb-gateway-prev"; exit 1; }
          docker rm -f vwb-gateway >/dev/null 2>&1 || true
          docker rename vwb-gateway-prev vwb-gateway && docker start vwb-gateway >/dev/null
          docker ps --filter name=vwb-gateway --format "rolled back: {{.Names}} {{.Image}} {{.Status}}"'
    exit 0
fi

# ---- 1. image ---------------------------------------------------------------------------
if [[ $MODE != --no-build ]]; then
    say "building $IMAGE (linux/arm/v7)"
    docker buildx build --platform linux/arm/v7 -t "$IMAGE" --load . 2>&1 | grep -E "error|ERROR|warning|Finished" || true
    docker builder prune -af >/dev/null 2>&1 || true
fi
docker image inspect "$IMAGE" >/dev/null || { echo "image $IMAGE not found"; exit 1; }
say "copying $IMAGE to $PFC"
docker save "$IMAGE" | $SSH docker load | sed 's/^/    /'

# ---- 2. config folder (never overwritten) ------------------------------------------------
if $SSH "test -f $VWB_DIR/config/gateway.toml"; then
    say "config: keeping $VWB_DIR/config"
else
    say "config: creating $VWB_DIR/config from config/ (edit the charger profile afterwards)"
    tar -C . -cf - config | $SSH "mkdir -p $VWB_DIR && tar -C $VWB_DIR -xf - && chmod 600 $VWB_DIR/config/gateway.toml"
fi

# ---- 3. container ----------------------------------------------------------------------
say "starting container vwb-gateway ($IMAGE)"
$SSH "set -e
    docker rm -f vwb-gateway-prev >/dev/null 2>&1 || true
    for c in \$(docker ps -a --format '{{.Names}}' | grep '^vwb-gateway-' || true); do docker rm -f \$c >/dev/null; done
    if docker inspect vwb-gateway >/dev/null 2>&1; then
        docker stop vwb-gateway >/dev/null; docker rename vwb-gateway vwb-gateway-prev
    fi
    docker run -d --name vwb-gateway $RUN_OPTS -v $VWB_DIR/config:/config $IMAGE >/dev/null
    sleep 8
    if [ \"\$(docker inspect -f '{{.State.Running}} {{.RestartCount}}' vwb-gateway)\" != 'true 0' ]; then
        echo '!!! new container does not keep running:'; docker logs --tail 15 vwb-gateway 2>&1 | sed 's/^/    /'
        docker rm -f vwb-gateway >/dev/null
        if docker inspect vwb-gateway-prev >/dev/null 2>&1; then
            docker rename vwb-gateway-prev vwb-gateway && docker start vwb-gateway >/dev/null && echo '!!! previous container started again'
        fi
        exit 1
    fi
    docker logs vwb-gateway 2>&1 | head -n 8 | sed 's/^/    /'"

# ---- 4. /walmgateway: set up by the container at its start (see the log lines above) ------

# ---- 5. check, clean up ----------------------------------------------------------------
code=$(curl -s -o /dev/null -m 5 -w '%{http_code}' "http://$PFC/walmgateway/api/status" || true)
[[ $code == 200 ]] && say "dashboard OK: http://$PFC/walmgateway/  (login admin / wago until changed)" \
                   || say "WARNING: http://$PFC/walmgateway/api/status answered '$code'"
$SSH "keep=\$(docker ps -a --format '{{.Image}}' | grep '^vwb-gateway:' | sort -u)
      for i in \$(docker images --format '{{.Repository}}:{{.Tag}}' | grep '^vwb-gateway:'); do
          echo \"\$keep\" | grep -qx \"\$i\" || docker rmi \$i >/dev/null 2>&1 && true
      done
      docker images --format '    image {{.Repository}}:{{.Tag}} {{.Size}}' | grep vwb-gateway
      docker ps -a --filter name=vwb-gateway --format '    {{.Names}} {{.Image}} {{.Status}}'"

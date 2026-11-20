# Cross-compiles a static armv7 (musl) binary on the build host — no QEMU needed —
# and ships it on a static busybox (~1.3 MB, for the start script) for the WAGO CC100 (linux/arm/v7).
#
#   docker buildx build --platform linux/arm/v7 -t vwb-gateway:0.1 --load .
#
# The builder always runs natively ($BUILDPLATFORM); only the final scratch image is arm/v7.

FROM --platform=$BUILDPLATFORM messense/rust-musl-cross:armv7-musleabihf AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --target armv7-unknown-linux-musleabihf

# busybox: sh, sed, nsenter for deploy/entrypoint.sh (sets up /walmgateway in the controller's
# lighttpd at every start, see deploy/walmgateway-check.sh)
FROM busybox:1.36.1-musl
COPY --from=build /app/target/armv7-unknown-linux-musleabihf/release/vwb-gateway /vwb-gateway
COPY config /config
COPY deploy/entrypoint.sh deploy/walmgateway-check.sh deploy/walmgateway-lighttpd.conf /usr/share/walmgateway/
EXPOSE 502
ENTRYPOINT ["/bin/sh", "/usr/share/walmgateway/entrypoint.sh"]
CMD ["/config/gateway.toml"]

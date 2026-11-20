# Cross-compiles a static armv7 (musl) binary on the build host — no QEMU needed —
# and ships it on scratch for the WAGO CC100 (linux/arm/v7).
#
#   docker buildx build --platform linux/arm/v7 -t vwb-gateway:0.1 --load .
#
# The builder always runs natively ($BUILDPLATFORM); only the final scratch image is arm/v7.

FROM --platform=$BUILDPLATFORM messense/rust-musl-cross:armv7-musleabihf AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --target armv7-unknown-linux-musleabihf

FROM scratch
COPY --from=build /app/target/armv7-unknown-linux-musleabihf/release/vwb-gateway /vwb-gateway
COPY config /config
EXPOSE 502
ENTRYPOINT ["/vwb-gateway", "/config/gateway.toml"]

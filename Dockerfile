FROM rust:1.96-slim-trixie AS builder
WORKDIR /usr/src/app
COPY . .
# Public core only: server binary + the two TRUSTED browser runtime modules,
# all built here from reviewed source. No indicator runner, no plugin .wasm,
# no private source/credentials/libraries. .dockerignore excludes any locally
# generated static/wasm-host/*.wasm, so a missing build can never be hidden.
RUN rustup target add wasm32-unknown-unknown \
 && cargo build --locked --release -p cvd-server \
 && cargo build --locked --profile wasm-release --target wasm32-unknown-unknown -p cvd-browser-host \
 && (cd crates/browser-coordinator && cargo build --locked --release --target wasm32-unknown-unknown --target-dir /usr/src/app/target/coordinator) \
 && sh scripts/assemble-static.sh /usr/src/app/static \
      target/wasm32-unknown-unknown/wasm-release/cvd_browser_host.wasm \
      target/coordinator/wasm32-unknown-unknown/release/cvd_browser_coordinator.wasm \
      /out/static

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates     && rm -rf /var/lib/apt/lists/*     && mkdir -p /app/data && chown 10001:10001 /app/data
WORKDIR /app
COPY --from=builder /usr/src/app/target/release/cvd /usr/local/bin/cvd
COPY --from=builder /out/static ./static
COPY config/cvd.container.toml ./config.toml
COPY LICENSE NOTICE THIRD_PARTY_LICENSES.txt ./
USER 10001:10001
ENV CVD_CONFIG=/app/config.toml CVD_OUTBOX_PATH=/app/data/outbox.sqlite
EXPOSE 8080
CMD ["cvd"]

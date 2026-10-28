FROM rust:1.96-slim-trixie AS builder
WORKDIR /usr/src/app
COPY . .
# Public host/runner only. No private source, credentials or libraries in this image.
RUN cargo build --locked --release -p cvd-server -p cvd-indicator-runner

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates     && rm -rf /var/lib/apt/lists/*     && mkdir -p /app/data && chown 10001:10001 /app/data
WORKDIR /app
COPY --from=builder /usr/src/app/target/release/cvd /usr/local/bin/cvd
COPY --from=builder /usr/src/app/target/release/cvd-indicator-runner /usr/local/bin/cvd-indicator-runner
COPY static ./static
COPY config/cvd.container.toml ./config.toml
COPY LICENSE NOTICE THIRD_PARTY_LICENSES.txt ./
USER 10001:10001
ENV CVD_CONFIG=/app/config.toml CVD_OUTBOX_PATH=/app/data/outbox.sqlite
EXPOSE 8080
CMD ["cvd"]

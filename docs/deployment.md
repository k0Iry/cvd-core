# Operator deployment (Linux)

The first supported host/runner target is Linux. Build native libraries for the
same OS, architecture and ABI as their runner. The versioned C-compatible ABI
is implemented in Rust; this is not Rust's unstable native library ABI.

## Public host image

```sh
docker build --tag cvd-core:local .
```

The image uses versioned Rust 1.96 / Debian trixie bases, includes only the
public host, public runner and static assets, and runs as UID/GID 10001 on
port 8080. It does not build or contain private libraries. `.dockerignore`
excludes Git history, local targets, libraries, secrets and runtime data.

Supply PostgreSQL via `DATABASE_URL`; do not bake its credentials into images.
Use a writable data volume owned by UID 10001 for the SQLite outbox. Mount an
operator TOML file over `/app/config.toml` read-only and trusted libraries at
an operator-owned location such as `/plugins`, also read-only. Start from
`config/cvd.container.toml`, preserving its non-root port and static path,
and add library entries as illustrated in `config/cvd.example.toml`.

Public CVD/Flow and private indicators are independent native builds. Adding a
configured library does not require rebuilding core. Restart/reconfigure the
service to change library configuration; there is no hot-unload mechanism.

Use a read-only root filesystem, a bounded temporary filesystem, dropped
capabilities and `no-new-privileges` where supported. Keep writable paths
limited to the outbox volume. Size container memory/PID/CPU limits for the
chosen number of sources/plugins, in addition to runner limits and deadlines.
The core image starts without plugins; install public CVD/Flow libraries to
show those plots. Market-data storage/cumulative values remain host-owned.

## Failure and trust boundaries

Only load libraries trusted by the operator. An independent runner contains
ordinary native crashes/hangs; it is not an arbitrary-code security sandbox.
Ingestion does not await indicator work. A failed/invalid computation is
unavailable, not a real zero or a silently substituted old result. A poisoned
non-finite input window stays unavailable until its required context is clean
or a valid replacement repairs the affected bucket. Normal finite windows
retain their original numerical semantics.

Public HTTP errors contain fixed codes/messages, not stderr, paths or panic
text. Keep bounded operator diagnostics private. Do not distribute private
source, oracle fixtures or debug/build artifacts in a public image/cache.

## Verification and rollout

Run core/Rust, frontend, real-runner and disposable-PostgreSQL tests before
rollout. Browser regression uses real Chromium and synthetic inputs with real
library outputs; it is not Safari or production-data acceptance. CI checks the
public-only image build. No production deployment is implied by source push.
Keep the prior production service/database intact until the operator's rollout
and live checks succeed; do not rewrite old cumulative history as migration.

## Licenses

Owned public code is Apache-2.0. Retain `LICENSE`, `NOTICE` and vendored
component notices, including TradingView attribution. Private libraries keep
their own terms; using this SDK does not automatically make them public.
Operators remain responsible for the terms governing their market-data feeds.

## Native indicator runner: platform contract (Linux)

The host requires Linux >= 5.11 for native runners: before exec it calls
`close_range(3, UINT_MAX, CLOSE_RANGE_CLOEXEC)` so no inherited parent
descriptor (e.g. CI/jobserver pipes) reaches the runner or its descendants;
stdin/stdout/stderr are preserved. If the kernel or a seccomp policy rejects
the syscall, runner spawn fails closed (indicator unavailable) rather than
leaking descriptors. This is fault isolation for trusted code, not a sandbox.

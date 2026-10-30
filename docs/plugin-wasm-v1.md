# Browser WASM plugin guide — `cvd-wasm` v1

A CVD indicator plugin is a standalone `wasm32-unknown-unknown` module. A
visitor imports it locally in the browser (Indicators → Import). The server
never loads, stores or runs plugins. It only serves base data. Plugins have no
imports, so they cannot reach the network.

This document is the normative contract for the WASM module. The JSON bodies
(descriptor, requests, output, errors) are defined in
[`plugin-api-v1.md`](plugin-api-v1.md). If the two disagree about module
profile, resources, lifecycle or errors, this document wins.

## 1. Quick start (Rust SDK)

Most authors only need this section.

1. Write the indicator with the SDK: `impl Indicator for T` and
   `export_indicator!(T)`. Follow the public
   [authoring guide](https://github.com/k0Iry/cvd-indicators/blob/master/docs/authoring.md)
   and the example
   [`examples/range`](https://github.com/k0Iry/cvd-indicators/tree/master/examples/range).
   (`tests/fixtures/plugins/sample-indicator` is a host test fixture, not a
   template.) [`plugin-api-v1.md`](plugin-api-v1.md) is retired as an
   architecture document; use it only for the shared JSON model:
   [Descriptor](plugin-api-v1.md#4-descriptor) and
   [Canonical input](plugin-api-v1.md#5-canonical-input-inputbar).
2. In the plugin's `Cargo.toml`, build a `cdylib`. Add the profile too; in a
   Cargo workspace, profiles only take effect in the workspace-root
   `Cargo.toml`:

   ```toml
   [lib]
   crate-type = ["cdylib", "rlib"]

   [profile.wasm-release]
   inherits = "release"
   panic = "abort"
   ```

3. In `.cargo/config.toml`, declare the memory maximum (512 MiB):

   ```toml
   [target.wasm32-unknown-unknown]
   rustflags = ["-C", "link-arg=--max-memory=536870912"]
   ```

4. Build:

   ```sh
   rustup target add wasm32-unknown-unknown
   cargo build --target wasm32-unknown-unknown --profile wasm-release
   ```

5. Import the `.wasm` file from
   `target/wasm32-unknown-unknown/wasm-release/` in the browser
   (Indicators → Import).

Do not use wasm-bindgen or JS glue. The SDK emits every required export for
you. The rest of this document matters only if you write a module without the
SDK, or want to know exactly what the host checks.

## 2. Base data your plugin sees

- Bars are canonical and renderable only, scoped to the requested source and
  timeframe.
- A missing or invalidated bucket is *missing*, not zero. The host never
  passes a window that bridges an invalid bucket; such a request ends as
  `unavailable` / `invalid_input_window` and earlier values are withdrawn.
- Cumulative CVD (`cvd_usdt`, aggregate `ac`) is computed by the server and is
  authoritative. Do not recompute it.

Network producer-epoch and invalidation recovery are the host's job. The
request revision and `seq` rules in §5 remain part of this ABI.

## 3. Module profile

The host checks the module in the plugin Worker before `WebAssembly.compile`.
Compile remains the final validity check.

- File ≤ 16 MiB; magic `\0asm`, version 1 (core module, not a component).
- No imports (`forbidden_import`). No start section (`forbidden_start`).
- Exactly one defined memory, exported as `memory`; not shared, not memory64;
  initial ≤ 256 pages (16 MiB); **declared max required**, ≤ 8192 pages
  (512 MiB) (`bad_memory`). The browser enforces the max on every grow.
- At most one table, `funcref` only; **declared max required**, ≤ 65 536 and
  ≥ min (`bad_table` / `resource_limit`).
- Counts: types ≤ 4096 (arity ≤ 64), functions ≤ 100 000, exports ≤ 64
  (`resource_limit`). Sections 0 (custom), 1–7 and 9–12 only; others →
  `unsupported_feature`.
- Exports: exactly the set below, plus optional globals `__data_end` and
  `__heap_base`. Others → `unexpected_export`; missing or wrong types →
  `missing_export` / `bad_signature`.

## 4. Exports (advanced ABI reference)

All scalars are `i32`.

| export | signature | meaning |
|---|---|---|
| `memory` | memory 0 | private linear memory |
| `cvd_wasm_abi_version` | `() -> i32` | returns 1 |
| `cvd_alloc` | `(len) -> ptr` | input buffer, `1..=32 MiB`; 0 = failure |
| `cvd_dealloc` | `(ptr, len)` | frees a `cvd_alloc` buffer |
| `cvd_describe` | `() -> status` | Descriptor JSON in the result slot |
| `cvd_create` | `(ptr, len) -> status` | creates the one instance from params JSON; a second call → 3 |
| `cvd_invoke` | `(op, ptr, len) -> status` | op 1 History, 2 LiveInit, 3 LiveUpdate |
| `cvd_destroy` | `() -> ()` | idempotent |
| `cvd_result_ptr` / `cvd_result_len` | `() -> i32` | location of the last result |

Status: 0 OK, 1 ERROR (`PluginError` JSON in the result slot), 2 PANIC
(reserved; a wasm panic is a trap), 3 INVALID_ARGUMENT.

**Buffers and results.**

- The host calls `cvd_alloc(n)`, checks `ptr != 0 && ptr + n ≤ memory size`,
  writes the input, and calls the export.
- The result slot is valid from the return of `cvd_describe` / `cvd_create` /
  `cvd_invoke`. Calling `cvd_result_ptr` / `cvd_result_len` does **not**
  invalidate it; any other subsequent export call, including `cvd_dealloc`,
  may. The host therefore **copies the result before** calling `cvd_dealloc`.
  A trap in `cvd_dealloc` is a fatal `trap`.
- A zero-length region passes the region check, and a successful `cvd_create`
  may return an empty result. A successful `cvd_describe` / `cvd_invoke` must
  still return valid Descriptor / Output JSON; empty bytes fail validation.
- Result regions are bounds-checked in 64-bit arithmetic; a bad region is the
  fatal `invalid_output_region`.

**Size limits.** Input ≤ 32 MiB, output ≤ 32 MiB, descriptor ≤ 256 KiB,
params ≤ 64 KiB, memory ≤ 512 MiB (checked after each call: `memory_limit`).

## 5. History and live semantics

- **History (op 1) is stateless.** It runs on a fresh instance built from the
  params given to `cvd_create`, then drops it. It never reads or changes the
  live instance. Identical requests must return identical bytes.
- **Live (op 2/3)** runs on the one persistent instance from `cvd_create`.
  LiveInit requires `descriptor.live`.
- Non-SDK modules must follow the same rule.

**Output shape.** Output must validate against the descriptor and the expected
times: `bars[output_from..]` for History, the last bar for LiveInit, and the
touched suffix of the live window for LiveUpdate.

**Live guards** (the host mirrors the SDK):

- the window is capped at `context.bars_for(tf) + 1 + LIVE_REPLACE_SLACK`;
- the revision is fixed per session (`stale_revision`);
- `seq` must be exactly last + 1 (`sequence_gap`);
- a replace must target a bar inside the window; if the earliest touched bar
  was already trimmed, the update fails (`delta exceeds live window`).

Host live state changes only after a validated OK. Rejections and
`plugin_error` leave it unchanged.

## 6. Runtime, failures and isolation

- Each instance runs in its own Worker with its own memory and an empty import
  object. No plugin JS is run, and untrusted bytes are never parsed on the UI
  thread. At most one request is in flight per instance (`busy`).
- Any trap is fatal: the Worker is terminated and the instance retired.
- Default deadlines: compile + validate 10 s; instantiate + describe +
  create 5 s; each compute 5 s. Overrides must be known keys in
  (0, 60000] ms (`bad_limits`). Expiry → `deadline` (fatal).
- Cancellation is physical. Superseding a running request rejects it with
  `superseded` and terminates that Worker.
- After any fatal error (`superseded`, `deadline`, `trap`, …) or `disable`,
  the instance is stale. The app must load it again and, for live, resend
  LiveInit before LiveUpdate. No old state survives.
- A plugin error is reported as `plugin_error` with `<public code>: <fixed
  message>`.

Error codes: `module_too_large, bad_magic, bad_module, malformed_module,
forbidden_import, forbidden_start, bad_memory, bad_table, resource_limit,
unsupported_feature, missing_export, bad_signature, unexpected_export,
abi_mismatch, bad_descriptor, invalid_output_region, invalid_output,
plugin_error, invalid_input, input_too_large, sequence_gap, stale_revision,
memory_limit, trap, deadline, poisoned, stale, busy, superseded, disabled,
replaced, worker_failed, post_failed, bad_limits, bad_command, lifecycle`.

Host internals (Worker messages, fence controller, host FFI) are not part of
the author contract. See `crates/browser-host` and
`static/wasm-host/host-bridge.js`.

## 7. Host foundation gate

From this repo's root (needs Rust, Node and Chromium):

```sh
bash scripts/wasm-foundation-gate.sh ../cvd-wasm-check
```

This checks this repo's own host and SDK fixtures and prints `GATE PASS`; it
does not test your plugin. `CVD_TARGET_DIR` optionally sets the Cargo target
dir.

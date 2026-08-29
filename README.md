> ⚠️ **Warning:** This project is currently in a rapid iteration prototype stage.
# Introduction
This project aims to create a lightweight, unified messaging application capable of connecting to any messaging platform you desire.

### Native support
- [x] Discord
- [x] Slack
- [x] Telegram
- [ ] Teams
- [ ] Signal
- [x] Steam

# Technical details
The core of the project is divided into two main components: adapters and the application.

The application crate is responsible for handling the user interface and coordinating communication with the adapter crates to send and receive messages across different messaging backends.

Adapters crate exposes all messaging backends as a Messenger object, providing a unified interface for the application to interact with various platforms.

## Development

The workspace pins its nightly toolchain in `rust-toolchain.toml` and its Rust
resolution in the tracked `Cargo.lock`. On Linux, install Nix and run commands
through the locked [devenv](https://devenv.sh/) environment. macOS builds also
need `opus` and `pkg-config`; the project CI covers Linux, macOS, and Windows.

```sh
# Build and validate the default (Discord) application.
devenv shell -- cargo check --workspace --all-targets --locked
devenv shell -- cargo test --workspace --all-targets --locked
devenv shell -- cargo clippy --workspace --all-targets --locked -- -D warnings

# Run it. Add `--features full` to compile the optional Steam/Slack/Telegram adapters.
devenv shell -- cargo run -p record_v2
```

## Credential storage

Saved account secrets use the native OS credential vault (Secret Service on
Linux, Keychain on macOS, and Credential Manager on Windows). `LoginInfo`
contains only opaque vault references after migration. New installs place this
metadata and its recovery sidecars in the platform per-user state directory.
Existing working-directory stores remain supported; startup fails closed if
both locations contain state rather than guessing which copy is newer.

Existing owner-only plaintext files are migrated transactionally: plaintext is
replaced only after every vault write succeeds. If the vault is unavailable,
Unix falls back to an owner-only `0600` plaintext store so sign-in can persist
credentials; other platforms fail closed. Settings shows a prominent warning
whenever plaintext storage is active and includes the sanitized OS-vault failure
that triggered an automatic fallback. Privacy & Security settings can retry
migration to the OS vault or explicitly select plaintext storage where
owner-only permissions are enforceable.

A versioned transaction journal binds account names, vault slots, and ordering,
then resolves interrupted writes to either complete old or complete new state.
Atomic writes flush a private temporary file, rename it over `LoginInfo`, and
synchronize its parent directory; stale crash leftovers are removed under the
interprocess lock. Revision-checked saves prevent concurrent app instances from
silently clobbering each other. Store directory must be user-owned and its path
may not traverse untrusted writable directories. Vault stores are never silently
downgraded or partially loaded. Settings also lists active and unavailable saved
accounts with failure codes and supports transactional local sign-out.

Formatting and documentation use the same environment:

```sh
devenv shell -- cargo fmt --all -- --check
RUSTDOCFLAGS='-D warnings' devenv shell -- cargo doc --workspace --no-deps --locked
```

## Video timing traces

Video capture, latest-frame queues, scaling, H264/AV1, Discord transport,
receive, and UI preview emit structured trace events on isolated
`video_timing` target. Enable that target without packet-level Discord traces:

```sh
RUST_LOG='video_timing=trace,discord=debug,record_v2=debug,info' \
  devenv shell -- cargo run -p record_v2
```

Each Discord-sent outbound frame prints one colored, multi-line report. Durations
use milliseconds and group capture, latest-frame queue, Discord scheduling,
encoding, and transport stages. Reports identify largest actionable component,
frame-budget ratio, and stale latest-frame queue replacements. Stage
subrows break down native wait/decode, normalization/scaling, channel locking,
worker queue/encode, DAVE, packetization, AEAD, pacing, and UDP send time.

Call sites still emit structured `video_timing` fields with process-wide
`frame_id` and microsecond integer values, allowing other tracing subscribers to
aggregate exact measurements. Avoid `discord=trace` during profiling:
packet-level formatting can perturb video pacing.

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

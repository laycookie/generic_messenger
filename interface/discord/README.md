# Discord interface

Discord user-client adapter for `messenger_interface`, including query, text,
voice, camera, and screen-share support.

## Authentication

The adapter supports token, email/password, and QR-code authentication. Token,
password, and optional two-factor-code fields are marked as secret so interactive
frontends mask them. Treat persisted credentials and Discord sessions as account
credentials: keep them private and never commit them.

## Gateway and transport safeguards

- The general gateway uses Discord Gateway v10; voice connections use Voice
  Gateway v9.
- Main-gateway reconnects Resume from the last fully applied dispatch, preserve
  queued interface events across transport generations, validate Discord's
  supplied Resume origin, and retry transient setup failures with bounded
  backoff. A forced Identify emits an authoritative-state invalidation.
- Heartbeats use randomized initial jitter and reject a missing acknowledgement
  at the next deadline after a short transport-dispatch race grace.
- General and voice WebSocket connect, Identify, and Hello operations have
  15-second deadlines. Voice UDP IP discovery has the same wall-clock bound in
  addition to its packet-count bound, so joining a call cannot wait forever on a
  silent endpoint.
- Each interface event route retains at most 4,096 events. Overflow invalidates
  all routes and emits one `StateInvalidated` control event instead of silently
  dropping a realtime delta. `record_v2` responds by recreating the messenger
  and taking a fresh authoritative snapshot.
- Voice session descriptions are validated before installation. The transport
  currently accepts `aead_xchacha20_poly1305_rtpsize` with an exact 32-byte key;
  unsupported modes, codecs, and malformed keys return errors rather than
  reaching panic-only crypto paths.
- Session keys and media-session identifiers are redacted from debug output, and
  raw voice-video payloads and IP-discovery packet bytes are not logged.

Shared HTTP fetching is provided by `messenger_downloader`. It bounds response
sizes and deadlines, strips query strings from diagnostic URLs, truncates and
escapes server error previews, and applies bounded jitter to rate-limit retries.

## Validation

From the workspace development shell:

```console
cargo test -p messenger_downloader -p discord
cargo clippy -p messenger_downloader -p discord --all-targets -- -D warnings
cargo check -p record_v2 -p messenger_downloader -p discord
```

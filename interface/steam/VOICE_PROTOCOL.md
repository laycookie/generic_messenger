# Steam Chat Voice — Protocol Notes

Reverse-engineering notes for voice **calls** in the Steam interface. Living
document — updated as we capture more. See also the [`gns`](src/gns/mod.rs)
module (media path) and [`voice.rs`](src/voice.rs) (signaling), plus the
`project_steam_voice_*` memory notes.

## TL;DR

- **Signaling rides standard CM service methods** — nothing proprietary about the
  carrier. We issue `ChatRoom.JoinVoiceChat`; the server then pushes
  `VoiceChatClient.*` notifications (carrier `k_EMsgServiceMethod` = 146).
- The **`VoiceChatClient`** service is **unpublished** (absent from
  `steam_vent_proto`, SteamDatabase/Protobufs, node-steam-user). We **recovered
  the two join-time notification layouts by capture** (below) — they're
  roster/status only, no media info.
- The **networking cert** (phase 1) now **works end-to-end** against live Steam —
  the gate was a *required* `app_id` at the CM layer (the closed client adds one;
  open GNS doesn't). See the cert section.
- **Media** (audio) is GNS P2P. **No rendezvous appears passively** even with a
  peer present and talking → the newcomer must *initiate* the GNS connection. So
  media is a substantial `gns-rs` build, not a capture-and-decode job.
  Unimplemented; see `gns/`.

## What's documented publicly (very little)

- **Steamworks "Steam Voice"** — <https://partner.steamgames.com/doc/features/voice>
  — documents the *in-game* voice API (`ISteamUser::GetVoice`/`DecompressVoice`)
  for game-integrated voice. This is **not** Steam Chat voice and does not apply.
- **Protobuf dumps** (SteamDatabase/Protobufs, DoctorMcKay/node-steam-user,
  SteamRE/SteamKit) only include the chat-room voice *entry points* in
  `steammessages_chat.steamclient.proto`:
  - `ChatRoom.JoinVoiceChat (CChatRoom_JoinVoiceChat_Request) -> CChatRoom_JoinVoiceChat_Response { voice_chatid: fixed64 }`
  - `ChatRoom.LeaveVoiceChat (...)`
  - `ChatRoomClient.NotifyShouldRejoinChatRoomVoiceChat`
  - There is **no `service VoiceChatClient`** in any public dump (checked
    SteamDatabase + node-steam-user `master`, 2026-06-14). The per-participant
    voice signaling is unpublished.

## Observed signaling flow (empirical)

Captured 2026-06-14 on entering a group voice channel — our account driving
through the app, a friend on the official Steam client already in the channel.
Relevant CM kinds: `146 = k_EMsgServiceMethod` (service-notification carrier),
`147 = k_EMsgServiceMethodResponse`.

1. **Client → server:** `ChatRoom.JoinVoiceChat`, sent as
   `k_EMsgServiceMethodCallFromClient`.
2. **Server → client:** response (kind 147) — `CChatRoom_JoinVoiceChat_Response`
   carrying `voice_chatid` (observed e.g. `10778520861647100800`).
3. **Server → client:** a burst of service notifications (kind 146):

   | job_name | in `steam_vent_proto`? | purpose (inferred) |
   |---|---|---|
   | `VoiceChatClient.NotifyUserJoinedVoiceChat#1` | ❌ | a participant joined the voice chat |
   | `VoiceChatClient.NotifyAllUsersVoiceStatus#1` | ❌ | full roster + per-user voice state (likely where connection/endpoint info lives) |
   | `ChatRoomClient.NotifyChatRoomGroupRoomsChange#1` | ✅ `CChatRoom_...GroupRoomsChange_Notification` | the group's room list changed (also drives the UI "channel created/removed" event) |

   Raw log excerpt:
   ```
   ... writing raw k_EMsgServiceMethodCallFromClient message
   ... processing message job_id=7 kind=MsgKind(147)            # JoinVoiceChat response
   ... processing notification job_name="VoiceChatClient.NotifyUserJoinedVoiceChat#1"
   ... processing notification job_name="VoiceChatClient.NotifyAllUsersVoiceStatus#1"
   ... Steam: joined group voice (signaling only) voice_chatid=10778520861647100800
   ... processing notification job_name="ChatRoomClient.NotifyChatRoomGroupRoomsChange#1"
   ```

**Not observed at all:** no media **rendezvous** (ICE/SNP endpoints, keys) in any
form, even with a peer present and talking. `NotifyAllUsersVoiceStatus` turned out
to be roster/status only (decoded below), and nothing else followed. Per *Media
transport*, the newcomer must initiate — so the rendezvous won't appear until we
send the first one.

## Decoded payloads (2026-06-14)

Recovered from captured bodies (the fork's `body=<hex>` logging + manual wire
decode; `voice_chatid` in field 1 matches the logged `JoinVoiceChat` response,
which anchors the parse). Field **names are inferred**; numbers/types are from the
wire.

```proto
// VoiceChatClient.NotifyUserJoinedVoiceChat#1
// 0980db9cda9dff949511980302110100100118cc94967e30fcf0a112
message CVoiceChatClient_NotifyUserJoinedVoiceChat_Notification {
    optional fixed64 voice_chatid  = 1;  // == JoinVoiceChat_Response.voice_chatid
    optional fixed64 user_steamid  = 2;  // the user who joined
    optional uint32  chat_id       = 3;
    optional uint32  chat_group_id = 6;
}

// VoiceChatClient.NotifyAllUsersVoiceStatus#1
// 0980db9cda9dff9495121a0980db9cda9dff94951121799f34010010011800200028003800
message CVoiceChatClient_NotifyAllUsersVoiceStatus_Notification {
    optional fixed64 voice_chatid = 1;
    repeated UserVoiceState users = 2;
    message UserVoiceState {
        optional fixed64 voice_chatid = 1;
        optional fixed64 user_steamid = 2;
        optional bool    f3 = 3;  // status flags — all 0 in capture
        optional bool    f4 = 4;  // (muted / speaking / deafened / ...?)
        optional bool    f5 = 5;
        optional bool    f7 = 7;
    }
}
```

### Key finding: these are roster/status only — no media info

**None of the captured messages contain any media-connection data** — no IP,
port, ICE candidate, cert, or key. And **no rendezvous of any kind arrived**
(neither a classic-EMsg `voice-capture: unrouted` line nor a further
notification) while both parties were in the call and talking. The peer never
initiated a media connection to us.

Conclusion: the media path can't be recovered by *passive* capture. The real
client does its own half of the setup after `JoinVoiceChat`. We tested the
leading hypothesis — that it first **requests a networking cert** to become
reachable — by implementing that request (see the next section). It round-trips
but is walled on `InvalidParam`, and even so **no rendezvous followed**, which
points at a deeper truth: as the *newcomer*, our client must **initiate** the GNS
connection, not wait to receive one (see *Media transport*).

## Networking cert (phase 1) — ✅ working

The GNS crypto handshake needs a Steam-signed networking cert over our identity
key. **Status: implemented and confirmed against live Steam** —
`Steam: obtained networking cert cert_len=137 ca_key_id=…`. See `gns/cert.rs`.

- **Wire:** `CMsgClientNetworkingCertRequest` (EMsg `k_EMsgClientNetworkingCertRequest`)
  → `CMsgClientNetworkingCertReply` (EMsg `k_EMsgClientNetworkingCertRequestResponse`
  = **5622**). Reply fields: `cert` (a signed `CMsgSteamDatagramCertificate`,
  ~137 bytes), `ca_key_id`, `ca_signature`.
- **Decoding the reply needs no proto fork.** `CMsgClientNetworkingCertReply`
  has no `RpcMessageWithKind` binding, so `conn.job::<Req, Reply>` won't compile.
  Work around it with a local newtype that impls the foreign `RpcMessage` +
  `RpcMessageWithKind` (orphan rule allows it on a *local* type); steam-vent's
  blanket `NetMessage` impl then makes it a valid job response. See `gns/cert.rs`
  (`NetworkingCertReply`). This refutes the old scaffold claim that it needs an
  extended proto crate.
- **`key_data` per Valve's `GetCertificateRequest`** (`csteamnetworkingsockets.cpp`):
  a `CMsgSteamDatagramCertificate` with `key_type = ED25519`, `key_data =` the
  **raw 32-byte** Ed25519 public key, and identity via `SteamNetworkingIdentityToProtobuf`
  (`identity_string = "steamid:N"` + `legacy_steam_id`). Server fills
  `time_created`/`time_expiry`/`app_ids`. (GNS wraps it in
  `CMsgSteamDatagramCertificateRequest`; the CM message appears to carry the bare
  cert.)
- **The gate was `app_id` — required at the CM layer.** Absent → `InvalidParam`,
  even though open-GNS `GetCertificateRequest` sets none, so the *closed* Steam
  client supplies one. `app_id = 480` (Spacewar) issues a cert. This is why the
  long `InvalidParam` hunt was confusing: the rejection was independent of
  `key_data` content (invalid raw-32-bytes and fully-valid blobs erred
  identically) because the missing `app_id` was checked regardless. **TODO:** 480
  may scope the cert to the wrong app — confirm the right value for *voice*
  (`gns/cert.rs`); test whether `0` / a voice-specific id also issues.
- **Gotcha:** `conn.job` uses tokio timers; panics `no reactor running` unless
  awaited inside `SteamMessenger::run` (async-compat). See
  `project_steam_vent_tokio_context`.

## Still to recover

- `VoiceChatClient`: `NotifyUserLeftVoiceChat`, `NotifyUserVoiceStatus`, and
  whatever carries the media rendezvous (likely emitted only once *we* initiate a
  GNS connection — see *Media transport* — and possibly gated on the cert).

## Media transport (unimplemented)

- Steam Chat voice media = **GNS P2P**: Opus over an ICE/STUN-negotiated UDP
  path, Valve's SNP framing, Curve25519 + AES-GCM. See `gns/` and the
  `project_steam_voice_no_media_transport` memory note.
- **The newcomer initiates.** No rendezvous arrives passively when we join, even
  with a peer present and talking — so our client must *open* the GNS connection
  (send the first `CMsgSteamNetworkingP2PRendezvous` ConnectRequest), not wait to
  receive one. This is why passive capture stalls at the media boundary.
- **Plan:** drive [`gns-rs`](https://github.com/hussein-aitlahcen/gns-rs) (wraps
  Valve's open-source GNS, supports *custom signaling*) rather than port SNP from
  C++. Feed it the recovered signaling, the networking cert, and our identity key.
- The prerequisite networking cert round-trips but is currently **walled on
  `InvalidParam`** (see the cert section) — `gns/cert.rs`.
- The SDR relay network is proprietary → only direct-P2P/ICE calls are reachable
  (symmetric-NAT/relay-only calls are out).

## Media build plan (`gns-rs`)

Scoping for the GNS media path — substantial; isolated in its own crate
(`crate/steam_gns`) because of the heavy C++ dependency.

- **Library:** `game-networking-sockets` 0.2 (safe `gns` wrapper) + its `-sys`
  FFI, which compiles Valve's open-source GNS via cargo. **✅ Builds in the Nix
  dev shell** (`crate/steam_gns`). Build prereqs, all added to `flake.nix`: clang,
  cmake, OpenSSL, **protobuf** (protoc + lib), **abseil-cpp**, and **libclang**
  via `LIBCLANG_PATH` (the `-sys` build uses bindgen).
- **Why it fits:** open-source GNS exposes exactly what we need —
  - **custom signaling** (`ConnectP2PCustomSignaling` / `ReceivedP2PCustomSignal`):
    the app delivers rendezvous blobs itself (we relay over the CM), so no SDR.
  - **external cert** (`SetCertificate(blob)`): takes the Steam-signed cert we
    already obtain (phase 1); `GetCertificateRequest` mirrors our hand-built blob.
  - **Confirmed:** the safe `gns` 0.2 wrapper is **IP-socket only** (connect-by-IP
    / listen + message pump; no P2P custom signaling, no `SetCertificate`). So the
    P2P + cert path must use **raw `gns-sys` FFI** (a C++ vtable interface — large
    and `unsafe`) or a forked/extended wrapper. Only the post-connect message
    send/recv could reuse the safe layer.
- **Shape:** (1) ✅ `gns-sys` builds in Nix; (2) create a GNS instance,
  set our identity (`steamid:N`) + the phase-1 cert; (3) `ConnectP2PCustomSignaling`
  to the peer, pumping rendezvous blobs through the CM and feeding peer blobs back
  via `ReceivedP2PCustomSignal`; (4) bridge Opus ↔ the audio graph
  (`AddAudioSource`/`AddAudioInput`).
- **Open unknowns:** the CM service method that *delivers* our rendezvous to the
  peer (discovered when we send the first one); whether the real peer accepts our
  Steam-signed cert via `SetCertificate`; the right cert `app_id` (parked TODO).

## Capture tooling (in the repo)

To reproduce/extend the captures. `steam-vent` **drops** service notifications
with no subscriber (they never reach `take_unprocessed`), and subscribing from
outside the crate needs a real `protobuf::Message` (can't be faked with a
raw-bytes newtype — `ServiceMethodRequest: protobuf::Message`). So bodies are
recovered two ways:

- **Live drain** — `session.rs`, gated by `STEAM_VOICE_CAPTURE=1`: each ~250ms
  poll drains `conn.take_unprocessed()` and logs every unrouted message's
  `emsg`/`proto`/hex. Catches classic-EMsg messages with payloads.
- **steam-vent fork** — `vendor/steam-vent` (via `[patch.crates-io]`): the message
  filter's `k_EMsgServiceMethod` branch logs the notification `body=<hex>` next to
  `job_name`, so the *dropped* `VoiceChatClient.*` notifications are dumped too.
- `record_v2/main.rs` honors `RUST_LOG` (was hardcoded before).

Run (joins are solo-testable; only the rendezvous needs a peer):

```text
STEAM_VOICE_CAPTURE=1 RUST_LOG=record_v2=info,steam=debug,steam_vent=debug,info \
  cargo run -p record_v2
```

Decode hex with `protoc --decode_raw` (or `xxd -r -p | protoc --decode_raw`). To
wire a recovered message in for real: write its `steammessages_voicechat` proto,
generate the type, `on_notification::<T>()` it, handle in `voice.rs`. For the UI
"voice call created/ended" event, subscribe to
`ChatRoomClient.NotifyChatRoomGroupRoomsChange` (already typed in
`steam_vent_proto`) plus the `VoiceChatClient` join/leave notifications.

## References

- Steamworks Steam Voice (in-game, not chat): <https://partner.steamgames.com/doc/features/voice>
- SteamDatabase/Protobufs: <https://github.com/SteamDatabase/Protobufs>
- node-steam-user protobufs: <https://github.com/DoctorMcKay/node-steam-user/tree/master/protobufs>
- SteamRE/SteamKit: <https://github.com/SteamRE/SteamKit>
- ValveSoftware/GameNetworkingSockets: <https://github.com/ValveSoftware/GameNetworkingSockets>

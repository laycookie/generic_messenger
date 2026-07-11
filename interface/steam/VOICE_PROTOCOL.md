# Steam Chat Voice — Protocol Notes & Implementation

Reverse-engineering notes and the implementation design for voice **calls** in the
Steam interface. Living document. See also the [`gns`](src/gns/mod.rs) module
(media path + transport seam), [`voice.rs`](src/voice.rs) (signaling), and the
`project_steam_voice_*` memory notes.

## TL;DR

Steam voice is **two planes**:

- A **signaling / control plane** that rides the Steam Connection-Manager (CM)
  socket `steam-vent` already holds. We issue `ChatRoom.JoinVoiceChat`; the server
  pushes `VoiceChatClient.*` notifications (carrier `k_EMsgServiceMethod` = 146).
  **This part largely works** — join/leave and the roster notifications — and
  nothing about the carrier is proprietary.
- A **media plane** that is a full **GameNetworkingSockets (GNS) peer-to-peer
  connection**: Opus audio inside Valve's SNP framing, inside an AES-256-GCM
  encrypted channel, over an ICE/STUN-negotiated **direct UDP** path. "Over GNS"
  means those four layers are GNS's job. **This part is unbuilt** and is the
  remaining work.

The realistic build is **not** to reimplement that transport — it is to drive
Valve's own open-source GNS (via `gns-rs`) and feed it three things we can
produce: the Steam-signed cert (done), a relay for its signaling blobs over the
CM, and an Opus ↔ audio-graph bridge. Almost everything hard is already correct
inside Valve's C++.

**What is implemented in this repo right now** (see *Implementation*): the **whole
voice pipeline, wired into the app and functional** — `Voice::connect`/`listen`
join the session, bring up a transport, and drive a per-call audio session
(mic → Opus → transport → Opus → playback) surfaced to the UI as
`CallStreamReady`. With the **loopback transport** (`STEAM_VOICE_LOOPBACK=1`),
joining a Steam voice channel echoes your own microphone back through the entire
real pipeline — a working voice call today. The one genuinely blocked joint is
`GnsTransport::connect` (the `gns-sys` FFI + the live rendezvous carrier) — i.e.
carrying that same audio to a *real Steam peer* instead of the loopback.

---

## ⚠️ The SDR barrier — reframing the goal (researched 2026-07-06)

A load-bearing correction. Steam Chat voice (2018 rewrite) is WebRTC-family
(Opus, encrypted) **but the media always relays through Steam Datagram Relay
(SDR), not direct P2P** — deliberately, for IP privacy. Valve, verbatim:

- Chat announcement: *"all traffic is sent through Steam servers rather than
  directly to peers. This keeps your IP address private."*
- SDR / GNS docs: *"on Steam they always relay traffic and do not share IP
  addresses between untrusted peers"*; *"the SDR support code is not opensource"*;
  SDR access is gated to Steam partners with a valid AppID.

**Consequence:** the direct-P2P/ICE transport work was paused and the old
isolated GNS build crate has been removed for now. The design is
still correct GNS, but it **cannot reach a retail Steam voice peer** — that peer
only speaks SDR, which open-source GNS cannot join. There are effectively no
direct-P2P Steam voice calls to intercept. So the earlier "only direct-P2P calls
are reachable" framing is **inverted**: direct-P2P interop with the official
client is *impossible*, not merely partial.

**Prior art:** SteamKit2, node-steam-user, steam-vent, and SteamDatabase have all
RE'd the *signaling* (chat rooms, the voice entry points; node-steam-user even has
legacy "started speaking" notifications) — but **no public third-party client
implements Steam chat voice media.** Everyone hits the SDR wall.

**Three tracks for "full" voice:**

- **A — voice between users of *this* client (achievable).** Direct GNS P2P over
  ICE + ephemeral certs, rendezvous relayed over the CM. Reuses everything we
  built. Trades IP privacy for reachability; does *not* interop with the official
  client. This is the realistic path.
- **B — interop with the official client's voice (blocked).** Needs SDR access
  (closed, partner-gated). Infeasible for a third party.
- **C — RE the SDR relay client (not recommended).** The message *formats* are in
  `steam_vent_proto` (all `CMsgSteamDatagram*`), but the relay-client protocol +
  POP access are the closed "SDR support code," tickets are gated, and it's almost
  certainly against Steam's Subscriber Agreement. A very large, live-gated,
  ToS-risky effort against infrastructure Valve actively controls.

The rest of this document describes the GNS media mechanics (accurate) and our
build. Read it as the **Track A** foundation; Track B is walled by SDR.

## The two planes

```
                 ┌──────────────────────── the app ────────────────────────┐
   control:      │  ChatRoom.JoinVoiceChat  ──▶  Steam CM (steam-vent)       │  ← works
   (signaling)   │  VoiceChatClient.* notifications  ◀──                     │
                 └──────────────────────────────────────────────────────────┘
                 ┌──────────────────────── the app ────────────────────────┐
   media:        │  Opus ⇄ SNP ⇄ AES-256-GCM ⇄ ICE/UDP  ⇄  the *peer*        │  ← unbuilt
   (GNS P2P)     │   (never touches the CM socket; direct peer-to-peer)      │
                 └──────────────────────────────────────────────────────────┘
```

The two planes are independent: the CM carries *who is in the call*; the GNS P2P
connection carries *the audio*, directly between the two Steam clients.

---

## Signaling plane (control) — mostly solved

### Observed flow (empirical, captured 2026-06-14)

On entering a group voice channel — our account driving through the app, a friend
on the official Steam client already in the channel. Relevant CM kinds:
`146 = k_EMsgServiceMethod` (service-notification carrier),
`147 = k_EMsgServiceMethodResponse`.

1. **Client → server:** `ChatRoom.JoinVoiceChat`, as
   `k_EMsgServiceMethodCallFromClient`.
2. **Server → client:** response (kind 147) — `CChatRoom_JoinVoiceChat_Response`
   carrying `voice_chatid` (observed e.g. `10778520861647100800`).
3. **Server → client:** a burst of service notifications (kind 146):

   | job_name | in `steam_vent_proto`? | purpose (inferred) |
   |---|---|---|
   | `VoiceChatClient.NotifyUserJoinedVoiceChat#1` | ❌ | a participant joined the voice chat |
   | `VoiceChatClient.NotifyAllUsersVoiceStatus#1` | ❌ | full roster + per-user voice state |
   | `ChatRoomClient.NotifyChatRoomGroupRoomsChange#1` | ✅ `CChatRoom_...GroupRoomsChange_Notification` | the group's room list changed |

The **`VoiceChatClient`** service is **unpublished** (absent from
`steam_vent_proto`, SteamDatabase/Protobufs, node-steam-user). We recovered the
two join-time layouts by capture (below) — they are **roster/status only**, no
media info.

### Decoded payloads

Recovered from captured bodies (the steam-vent fork's `body=<hex>` logging +
manual wire decode; `voice_chatid` in field 1 matches the logged `JoinVoiceChat`
response, which anchors the parse). Field **names are inferred**; numbers/types
are from the wire.

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

**Key finding:** none of the captured messages contain any media-connection data
(no IP, port, ICE candidate, cert, or key), and **no media rendezvous of any kind
arrived** while both parties were in the call and talking. The peer never
initiated a media connection to us. The media path therefore **cannot be
recovered by passive capture** — see *Why the newcomer must initiate*.

---

## Media plane (audio) — how GNS P2P works

This is the unbuilt half. Understanding it is the point of these notes.

### What "over GNS" means: the layer cake

GameNetworkingSockets is Valve's transport library. A P2P voice connection is
four stacked layers, **all implemented inside GNS**, bottom to top:

1. **ICE / STUN — NAT traversal.** Each peer gathers *candidate* addresses (its
   local IP:port, and its STUN-reflexive public IP:port), trickles them to the
   other, and probes candidate pairs with STUN connectivity checks until it finds
   a UDP path that works through both NATs. Valve also has an **SDR relay**
   fallback, but SDR is proprietary and unreachable from open-source GNS — so
   **only direct-P2P-reachable calls will ever work**; symmetric-NAT peers that
   can only relay are out.
2. **Crypto session — Curve25519 + Ed25519 + AES-256-GCM.** Each peer presents a
   **Steam-signed networking certificate** (an Ed25519 identity key Steam vouches
   for). Inside the handshake each side sends *signed session crypt info* — a
   Curve25519 ephemeral public key + nonce + cipher list — signed by its cert.
   The two Curve25519 keys do an ECDH exchange to derive a shared secret; key
   derivation and per-packet IVs follow Google's QUIC design; every packet is then
   AES-256-GCM. **Ed25519 authenticates *who you are*; Curve25519 establishes
   *the shared key*.**
3. **SNP (Steam Networking Protocol).** Valve's "QUIC-lite for games" running on
   the encrypted channel: fragments long messages, reassembles, coalesces short
   ones, sequences, and retransmits *reliable* fragments. Voice frames ride as
   **unreliable** messages (a dropped 20 ms Opus frame isn't worth retransmitting).
4. **Opus media.** 48 kHz stereo Opus frames — same codec and rate as Discord
   voice, so our existing encode/decode is a direct template.

### The establishment handshake

The message that ties the media plane together is
`CMsgSteamNetworkingP2PRendezvous`. To go from "joined the voice chat" to "audio
flowing":

1. **Get a networking cert** (phase 1). Ask Steam to sign your Ed25519 identity.
   *This works today* — see *Networking cert*.
2. **Initiator sends a rendezvous** with a `ConnectRequest` sub-message carrying
   its signed `cert`, its signed `crypt` (`CMsgSteamDatagramSessionCryptInfoSigned`
   = Curve25519 pubkey + nonce + `AES-256-GCM`), and `ice_enabled = true`.
3. **Peer replies** with a `ConnectOK` carrying *its* cert + crypt.
4. **ICE candidates trickle** as `CMsgICECandidate` (inside `CMsgICERendezvous`,
   in the rendezvous `reliable_messages`) until a working UDP pair is found.
5. **Crypto completes** (ECDH → AES keys), **SNP comes up**, and Opus frames flow
   as unreliable messages.

Rendezvous envelope fields relevant to us (from Valve's
`steamnetworkingsockets_messages.proto`):

| field | meaning |
|---|---|
| `from_identity`, `to_identity`, `*_connection_id` | who ↔ who |
| `connect_request { cert, crypt, *_virtual_port }` | initiate |
| `connect_ok { cert, crypt }` | accept |
| `connection_closed { reason_code, debug }` | tear down |
| `ice_enabled` | turn on ICE candidate negotiation |
| `reliable_messages[] → CMsgICERendezvous → CMsgICECandidate { candidate }` | trickled candidates |
| `sdr_routes`, `hosted_server_ticket` | relay/dedicated-server paths (**out of reach**) |

### The pivotal insight: who relays the rendezvous

**GNS does not know how to reach the peer's signaling.** In the retail Steam
client, GNS calls back into Steam, which relays the rendezvous through Valve's
backend. In open-source GNS you use **custom signaling**:

- You call `ConnectP2PCustomSignaling`. Whenever GNS wants to send a rendezvous it
  invokes your `SendSignal(blob)` callback; **you** deliver those opaque bytes to
  the peer over whatever channel you have.
- When a blob arrives from the peer you hand it to `ReceivedP2PCustomSignal(blob)`.

The signaling channel is just a dumb pipe for opaque blobs. **For us that pipe is
the CM socket steam-vent already owns** — we relay the rendezvous as a service
method, exactly as the retail client relays through Steam's backend. This is why
the problem is tractable: we don't invent the rendezvous, GNS generates it; we
just carry it.

### Why the newcomer must initiate

No rendezvous ever arrived passively, even with a friend in the call talking.
That is not a capture failure — **the newcomer initiates.** The peer won't
rendezvous to us until *we* open the connection (send the first `ConnectRequest`).
So the specific CM service method that carries an *outgoing* rendezvous is only
discoverable by sending the first one live, which needs a working GNS instance
driving `ConnectP2PCustomSignaling`.

---

## Networking cert (phase 1) — ✅ working

The GNS crypto handshake needs a Steam-signed cert over our identity key.
**Implemented and confirmed against live Steam** —
`Steam: obtained networking cert cert_len=137 ca_key_id=…`. See
[`gns/cert.rs`](src/gns/cert.rs).

- **Wire:** `CMsgClientNetworkingCertRequest` (`k_EMsgClientNetworkingCertRequest`)
  → `CMsgClientNetworkingCertReply` (`k_EMsgClientNetworkingCertRequestResponse`
  = **5622**). Reply: `cert` (a signed `CMsgSteamDatagramCertificate`, ~137 bytes),
  `ca_key_id`, `ca_signature`.
- **No proto fork needed.** `CMsgClientNetworkingCertReply` has no
  `RpcMessageWithKind` binding, so `conn.job::<Req, Reply>` won't compile. A local
  newtype (`NetworkingCertReply`) may impl the foreign `RpcMessage` +
  `RpcMessageWithKind` (orphan rule allows it on a *local* type); steam-vent's
  blanket `NetMessage` impl then makes it a valid job response.
- **`key_data`** (per Valve's `GetCertificateRequest`): a bare
  `CMsgSteamDatagramCertificate` with `key_type = ED25519`, `key_data =` the raw
  32-byte Ed25519 public key, and identity via `identity_string = "steamid:N"` +
  `legacy_steam_id`. Server fills time/app_ids and signs it.
- **The gate was `app_id` — required at the CM layer.** Absent → `InvalidParam`,
  even though open-GNS `GetCertificateRequest` sets none (the *closed* client
  supplies one). `app_id = 480` (Spacewar) issues a cert. **TODO:** 480 may scope
  the cert to the wrong app — confirm the right value for voice.
- **Gotcha:** `conn.job` uses tokio timers; panics `no reactor running` unless
  awaited inside `SteamMessenger::run` (async-compat). See
  `project_steam_vent_tokio_context`.

---

## Implementation (this repo)

The media plane is split at a **`VoiceTransport` seam** so the audio work is
decoupled from the blocked GNS transport. Everything above the seam is built,
wired into the app, and functional. Layout under [`src/gns/`](src/gns/) plus
[`voice.rs`](src/voice.rs):

```
voice.rs         impl Voice: connect (join + bring-up + CallStreamReady),
                 disconnect (kill + leave), listen (VoiceEvent stream)
gns/
  mod.rs         Phase enum + establish_voice_media() sequencer + GnsError
  cert.rs        Phase 1 — Steam-signed networking cert           ✅ live-confirmed
  signaling.rs   Phase 2 — rendezvous model (ConnectRequest/OK/ICE), typed
  transport.rs   VoiceTransport trait + VoiceFrame + GnsTransport + LoopbackTransport
  audio.rs       Opus ⇄ audio-graph bridge (encode/decode)         ✅ tested (loopback)
  call.rs        SteamAudioSession (AudioEvent stream, the media   ✅ tested (mic req + kill)
                 pump + channel acquisition) + VoiceEventStream
```

### App integration — functional

`record_v2` is messenger-agnostic for voice: it already calls `voice().listen()`,
handles `CallStreamReady` by pumping the `AudioEvent` stream, and creates the
mixer channels (F32 mic, I16 playback). So no app change was needed. The Steam
side now:

- `listen()` returns a real `VoiceEvent` stream (`call::VoiceEventStream`).
- `connect()` joins the room, builds a transport, spawns a
  `call::SteamAudioSession`, and pushes `CallStreamReady(audio_stream)` to the UI
  — which transitions the call to *Connected* and starts pumping audio.
- The session uses Discord's proven **two-phase "yield"** to acquire mixer
  channels: queue an `AddAudioInput`/`AddAudioSource` (with a oneshot), return so
  `ArcStream::next` delivers it to the UI, adopt the fulfilled channel next poll.
- `disconnect()` kills the session (ends the stream, drops the channels) and
  leaves the voice chat.

**Loopback demo — a working call today.** `STEAM_VOICE_LOOPBACK=1` selects the
loopback transport, so joining a Steam voice channel plays your own mic back
through the full real pipeline (mic capture → gate → Opus encode → transport →
Opus decode → mixer playback). This is functional voice end-to-end; only the peer
on the far side is yourself instead of another Steam user.

### The `VoiceTransport` seam (`transport.rs`)

```rust
trait VoiceTransport {
    async fn send(&self, opus_frame: &[u8]) -> Result<(), TransportError>;
    async fn recv(&self) -> Result<VoiceFrame, TransportError>; // { peer: steamid, opus }
}
```

- `GnsTransport` — the real impl. `connect()` runs cert (done) → rendezvous → ICE
  → SNP. **Blocked** on the `gns-sys` FFI + the live rendezvous carrier; returns
  the precise `GnsError` today. This is the *only* genuinely blocked joint.
- `LoopbackTransport` (test-only) — an in-memory echo: what you `send` you `recv`,
  tagged as coming from a configured peer. Lets the whole bridge be exercised
  without a network.

### The Opus bridge (`audio.rs`)

Transport-agnostic. Mirrors Discord's `gateways/voice/audio.rs`:

- Pulls a 48 kHz stereo frame (960 samples/ch, 20 ms) from a **`MicSource`**
  (production impl wraps `simple_audio_channels` `SampleConsumer`; partial fills
  persist across select-cancellation, so it's cancel-safe), Opus-encodes it, and
  `transport.send`s it.
- `transport.recv`s peers' frames, decodes each with a **per-peer** `opus::Decoder`
  (Opus carries inter-frame state, so speakers can't share one), and pushes the
  PCM to a **`PeerSinks`** (production impl = a per-peer `SampleProducer` from
  `AddAudioSource`; decoded audio plays through the mixer).
- Both directions run in one `futures::select!` loop.

**Tested** (`cargo test -p steam`): the loopback transport round-trips frames;
Opus encode→decode of a sine preserves the signal; and the full bridge, driven by
a fake mic over the loopback transport, delivers decoded audio to the sink. So the
audio plane is proven — only the transport under it is missing.

### The GNS transport — paused

The old isolated C++/FFI GNS build crate has been removed while
Steam voice media is paused. The remaining `interface/steam/src/gns` code keeps
the cert, signaling, and audio-session skeletons, but there is no active
`game-networking-sockets` dependency or local GNS transport build in the
workspace.

If this work resumes, the next transport attempt still needs:

1. **Certificate** — `SteamAPI_ISteamNetworkingSockets_SetCertificate(phase1 cert)`
   and configuring GNS to trust Steam's networking CA so it accepts the peer's
   cert. (The cert interop + `app_id` scoping are the biggest unknowns.)
2. **CM signaling relay** — implement `SignalRelay` to carry `SendSignal` blobs
   over the Steam CM (discover the service method by sending the first one live),
   and feed received rendezvous into `received_signal`.
3. **Wire into `interface/steam`** — adapt `GnsP2pConnection` to the
   `VoiceTransport` trait (behind a Cargo feature, since it pulls the C++ build)
   so `voice.rs` drives the audio session over it. Everything above the transport
   already works.

Note: standalone GNS is one instance per process, so a full custom-signaling
round-trip can't be tested in-process — it needs the real peer (step 2) or a
two-process harness.

Minor follow-up (independent of the transport): `listen()` currently emits only
`CallStreamReady`; wire `ParticipantJoined`/`Left` from the `VoiceChatClient.*`
roster notifications for the in-call participant list.

### Risks before that code is worth writing

- **Cert acceptance is unproven.** Open-source GNS manages its own certs; whether
  a *retail* Steam peer accepts our Steam-signed cert via `SetCertificate` — and
  whether `app_id 480` scopes it right for voice — is untested and is the biggest
  "does this even work" question.
- **The rendezvous carrier is only findable live** (send one, watch the response).
- **SDR-only peers are permanently out** (proprietary relay). Direct-P2P only.
- **None of it is exercisable here** — needs live creds + a friend on the retail
  client, with us as the newcomer who initiates.

The cheapest *next* experiment is not code: it's one live run that sends a single
GNS-generated rendezvous and sees whether Steam relays it and whether the peer
answers (settling the cert-acceptance risk).

---

## Still to recover

- `VoiceChatClient`: `NotifyUserLeftVoiceChat`, `NotifyUserVoiceStatus`, and
  whatever carries / triggers the media rendezvous once *we* initiate.
- The CM service method that delivers an outgoing `CMsgSteamNetworkingP2PRendezvous`
  to the peer.

## Capture tooling (in the repo)

`steam-vent` **drops** service notifications with no subscriber (they never reach
`take_unprocessed`), and subscribing from outside the crate needs a real
`protobuf::Message`. So bodies are recovered two ways:

- **Live drain** — `session.rs`, gated by `STEAM_VOICE_CAPTURE=1`: each ~250 ms
  poll drains `conn.take_unprocessed()` and logs every unrouted message's
  `emsg`/`proto`/hex. Catches classic-EMsg payloads.
- **Historical steam-vent fork** — the local patch that logged
  dropped `k_EMsgServiceMethod` bodies was removed while voice media is paused.
  The workspace currently uses the published `steam-vent` crate, so this extra
  service-method body dump is not available unless a capture branch reintroduces
  it.

Run (joins are solo-testable; only the rendezvous needs a peer):

```text
STEAM_VOICE_CAPTURE=1 RUST_LOG=record_v2=info,steam=debug,steam_vent=debug,info \
  cargo run -p record_v2
```

Decode hex with `protoc --decode_raw` (or `xxd -r -p | protoc --decode_raw`).

## References

- Steamworks Steam Voice (in-game, **not** chat voice): <https://partner.steamgames.com/doc/features/voice>
- SteamDatabase/Protobufs: <https://github.com/SteamDatabase/Protobufs>
- node-steam-user protobufs: <https://github.com/DoctorMcKay/node-steam-user/tree/master/protobufs>
- ValveSoftware/GameNetworkingSockets: <https://github.com/ValveSoftware/GameNetworkingSockets>
  - `steamnetworkingsockets_messages.proto` (rendezvous/crypt), `examples/trivial_signaling_client.cpp` (custom signaling)
- `gns-rs` (safe + `-sys` FFI wrapper): <https://github.com/hussein-aitlahcen/gns-rs>

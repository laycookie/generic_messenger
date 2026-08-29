# `messenger_interface`

Shared contract between messenger backends and the application.

Backends implement platform-specific authentication, queries, text chat,
audio calls, and video transport. The application talks only to these traits
and shared data types; it never handles Discord opcodes, Steam RPCs, codecs, or
other backend details.

## Mental model

`Messenger` is the backend identity and capability root. A concrete backend
implements any supported feature traits:

```text
Messenger
├── Query
├── Text
│   ├── TextEdit                 optional operation capability
│   ├── TextDelete               optional operation capability
│   ├── TextCancelSend            optional operation capability
│   └── TextReactions             optional operation capability
├── Audio
│   └── AudioSelfVoiceState      optional operation capability
└── Video
```

Feature support is discovered synchronously through generated caster traits.
No network call or trial operation is needed:

```rust,ignore
let audio = messenger.audio().ok();
let edit = messenger.text().ok().and_then(|text| text.edit().ok());
```

Optional capability traits use `<ParentFeature><Capability>` names, such as
`TextDelete` for an operation requiring `Text`.

Casters provide borrowed capability objects for ordinary operations and
retained capability guards for root event polling. Retained projection consumes
one lease upgrade without creating cloneable backend ownership.

Caster implementations use Rust specialization and are generated
automatically. Backend code implements feature/capability traits, not caster
traits.

## Identifiers and locations

`Identifier<D>` pairs a shared `u64` ID with typed data `D`. Equality compares
IDs even when the attached data types differ. Common locations are:

- `Identifier<Place<Room>>`: room/channel with display metadata and room data.
- `Identifier<Place<House>>`: server/guild/workspace with display metadata.
- `Identifier<User>` and `Identifier<Message>`: typed user/message values.
- `Identifier<()>`: ID-only reference used by events that do not need to copy
  full data.

Adapters own mappings between these shared IDs and native backend IDs.

## `Messenger`

Root trait implemented by every backend.

| Method | Purpose |
| --- | --- |
| `create_messenger(cred)` | Construct authenticated backend as unique `Owner<dyn Messenger>`. |
| `auth_methods()` | Declare login forms the UI can render. |
| `name()` | Return human-readable backend name. |
| `id()` | Return stable identity for this backend/account instance. Used for equality and registry keys. |
| `auth()` | Return credential subset safe and sufficient to persist and restore through `create_messenger`. |

Whole-feature casters:

| Caster trait | Borrowed method | Retained method |
| --- | --- | --- |
| `MessengerCasterQuery` | `query()` | `retained_query()` |
| `MessengerCasterText` | `text()` | `retained_text()` |
| `MessengerCasterAudio` | `audio()` | `retained_audio()` |
| `MessengerCasterVideo` | `video()` | `retained_video()` |

Each caster returns `MessengerError::NotImplemented` when concrete backend
does not implement that feature trait. Retained casters consume
`Retained<dyn Messenger>` and re-erase same allocation as corresponding
feature capability; no `Any` downcast or second allocation occurs.

## `Query`

Read-only state fetching. Mutations belong to more specific traits.

| Method | Purpose | Default |
| --- | --- | --- |
| `client_user()` | Fetch authenticated account/profile. | `NotImplemented` |
| `contacts()` | Fetch friends, contacts, or backend equivalent. | `NotImplemented` |
| `rooms()` | Fetch available rooms/channels. | `NotImplemented` |
| `houses()` | Fetch available servers/guilds/workspaces. | `NotImplemented` |
| `room_details(room)` | Fetch expanded room data. | `NotImplemented` |
| `house_details(house)` | Fetch expanded house data. | `NotImplemented` |
| `inspect_user(user)` | Fetch backend-specific user detail sheet. | `NotImplemented` |
| `inspect_house(house)` | Fetch backend-specific house detail sheet. | `NotImplemented` |
| `prepare_listener()` | Prepare query event delivery. | `NotImplemented` |
| `next_event()` | Wait for next query delta after listener preparation. | `None` |

## `Text`

Text history and sending form the required core. Other mutations are optional
capability traits.

| Method | Purpose | Default |
| --- | --- | --- |
| `get_messages(location, before, ordering)` | Fetch messages, optionally before a cursor. `Ordering::Time` requests oldest-first output. | Required |
| `pickable_emojis(location)` | Fetch custom emoji available to compose in this room. Unicode emoji are not included. | `NotImplemented` |
| `pickable_stickers(location)` | Fetch stickers available to send in this room. | `NotImplemented` |
| `send_message(location, contents)` | Send message and return server-confirmed message with final ID. | Required |
| `prepare_listener()` | Prepare text event delivery. | `NotImplemented` |
| `next_event()` | Wait for next message/reaction delta. Initial state/history still comes from `get_messages`. | `None` |

### Text operation capabilities

| Trait | Caster methods | Trait methods |
| --- | --- | --- |
| `TextEdit` | `edit()` | `edit_message(location, message, contents)` replaces content and returns server-confirmed message. |
| `TextDelete` | `delete()` | `delete_message(location, message)` deletes a sent message. |
| `TextCancelSend` | `cancel()` | `cancel_window()` returns eligibility duration; `cancel_send(location, message)` retracts within that window. |
| `TextReactions` | `reactions()` | `add_reaction(...)` and `remove_reaction(...)` mutate current user's reaction. |

`TextCancelSend` differs from `TextDelete`: cancel is a short-lived
undo-send operation; delete follows normal backend permissions and lifetime.

## `Audio`

Audio call lifecycle and event subscription. PCM crosses dedicated audio
channels, never `VoiceEvent`.

| Method | Purpose | Default |
| --- | --- | --- |
| `connect(location)` | Request voice connection. Returns current non-connected `CallStatus`; final readiness arrives asynchronously as `VoiceEvent::CallStreamReady`. | Required |
| `disconnect(location)` | Leave voice location. Trait intentionally makes teardown best-effort and errorless. | Required |
| `prepare_listener()` | Prepare backend-wide voice event delivery. | `NotImplemented` |
| `next_event()` | Wait for next backend-wide `VoiceEvent`. | `None` |

### How audio event polling works

Owner-aware wrapper prepares audio once, then returns one directly-driven root
stream:

```text
Owner<dyn Messenger>
└── LeaseStream<VoiceEvent>
    └── Lease<dyn Messenger>::retain()
        └── Retained<dyn Messenger>::retained_audio()
            └── Retained<dyn Audio>::next_event()
                ├── CallStatusUpdate
                ├── ParticipantJoined / ParticipantLeft
                └── CallStreamReady
                    └── WeakStreamSource<AudioEvent>
                        └── StreamScope::bind(...)
                            └── WeakSocketStream<AudioEvent>
                                ├── AddAudioSource
                                └── AddAudioInput
```

Outer `VoiceEvent` stream is control plane for all calls owned by backend:

- `CallStatusUpdate` reports connecting/failure progress.
- `ParticipantJoined` and `ParticipantLeft` maintain live call roster.
- `CallStreamReady` marks call connected and supplies inner, per-call
  `AudioEvent` stream.

Inner `AudioEvent` stream connects backend media transport to application
audio graph:

- `AddAudioSource` asks application to create playback output. Application
  sends `SampleProducer` back through included one-shot channel; backend pushes
  decoded remote samples into it.
- `AddAudioInput` asks application to create microphone input. Application
  sends `SampleConsumer` back; backend pulls samples for encoding.

Root stream owns only erased poll closure, weak lease, closure signal, and
in-flight event future. Each poll receives one non-cloneable retained guard.
Independent nested source remains Arc-owned and binds to same logical messenger
lifetime only when application opens it.

### `AudioSelfVoiceState`

Optional signaling capability for own mute/deafen flags.

| Method | Purpose |
| --- | --- |
| `set_self_voice_state(location, state)` | Publish own voice flags. Application remains responsible for actually muting microphone/playback. |

`AudioCasterSelfState` exposes `self_state()`.
Confirmed state returns through `VoiceEvent::ParticipantJoined` for current
user.

## `Video`

Webcam and screen-share lifecycle. `VideoBroadcastKind` identifies one stream;
`VideoBroadcast` bitflags represent all stream kinds participant currently
broadcasts, allowing camera and screen share simultaneously.

| Method | Purpose | Default |
| --- | --- | --- |
| `connect(location)` | Establish video feature in location. Shared-transport backends may reuse audio call connection. | Required |
| `disconnect(location)` | End video feature without necessarily ending shared audio call. | Required |
| `start_video(location, kind)` | Announce/start own camera or screen broadcast. | Required |
| `stop_video(location, kind)` | Stop one own broadcast kind. | Required |
| `subscribe_video(stream)` | Explicitly request advertised remote stream by opaque ID. Automatic-delivery backends need not override it. | `NotImplemented` |
| `unsubscribe_video(stream)` | Release explicit native subscription before remote stream ends. | `NotImplemented` |
| `prepare_listener()` | Prepare video event delivery. | `NotImplemented` |
| `next_event()` | Wait for next stream lifecycle or frame-channel handshake event. | `None` |

Video event flow:

- `VideoStreamAvailable`: remote stream exists but requires explicit
  `subscribe_video`.
- `AddVideoStream`: media delivery starts with decoded `initial_frame`.
  Application displays it immediately, creates `frame_channel()`, returns
  `FrameProducer`, then displays later frames from `FrameConsumer`.
- `RemoveVideoStream`: advertisement or active stream ended.
- `AddVideoInput`: backend requests camera/screen input for `start_video`.
  Application returns `VideoInput`, containing `FrameConsumer` and selected
  frame rate; capture pushes `VideoFrame` values through `FrameProducer`.

Decoded frames use `PixelData`; codec encode/decode remains backend/video-crate
work.

`VideoStreamId` is opaque and scoped to one backend instance. Stream metadata
(`user`, `location`, and `kind`) is announced separately and remains immutable
for that stream lifetime. Applications store both in a registry keyed by
`(backend ID, stream ID)`; subscription and removal operations pass only the
stream ID. Backend-specific connection data remains in the backend's private
active-stream registry.

## Event-stream mechanics

### Root feature streams

`MessengerInterface::listen_*` calls `LeaseStream::open` directly from stored
`Owner<dyn Messenger>`. Listener setup races owner closure. Once prepared,
returned `LeaseStream<Event>` is owned and driven by application until it ends
or is dropped; interface stores no stream, source `Arc`, or `StreamScope`.

On each root poll, `LeaseStream<Event>`:

1. checks owner closure;
2. upgrades erased `Lease<dyn Messenger>` into
   `Retained<dyn Messenger>`;
3. projects retained guard through feature caster;
4. calls owning `next_event(self: Retained<Self>)`;
5. yields event and drops guard, or lets backend transfer guard into committed
   cancellation-shielded work;
6. ends permanently when owner retires or `next_event` returns `None`.

Backends processing multiple operations inside one event poll check
`Retained::is_active()` before selecting another operation; retained access is
not itself revocation.

Owner is erased before stream creates its lease. Concrete backend type never
appears in `LeaseStream<Event>`, and retained projection uses trait vtable
rather than `Any`.

### Nested shared sources

`ArcStream`, `WeakStreamSource`, `StreamScope`, and `WeakSocketStream`
remain low-level primitives for independently shared nested sources such as
per-call audio. `StreamScope::bind` binds source availability to messenger
closure when application opens nested source.

| Method | Purpose |
| --- | --- |
| `ArcStream::next(self: Arc<Self>)` | Await next nested item using shared ownership; return `None` to end stream. |

Each root or nested stream is intended for one active consumer unless backend
explicitly documents broadcast behavior. A backend that detects a gap in its
realtime delta stream emits `QueryEvent::StateInvalidated`; consumers must take
a fresh authoritative snapshot before applying later events.

## Implementing a backend

1. Implement `Messenger`; construct concrete backend with `Owner::new`, then
   coerce directly to `Owner<dyn Messenger>`.
2. Implement supported whole-feature traits (`Query`, `Text`, `Audio`,
   `Video`). Generated borrowed and retained casters expose them automatically.
3. Implement optional operation traits only when support is real and usable.
4. Implement `prepare_listener` and owning `next_event` for each supported root
   event area. Move received `Retained<Self>` into committed work when external
   input has been consumed and cancellation can no longer discard it.
5. Use `ArcStream` plus `WeakStreamSource` only for independently shared nested
   sources such as per-call audio.
6. Keep native IDs/codecs/protocol payloads inside adapter.
7. Return shared `Identifier`, event, audio-channel, and video-frame types at
   interface boundary.

Backend crates overriding owning event methods enable nightly
`arbitrary_self_types`; workspace toolchain is pinned accordingly.

## Design notes

- [Communication models](docs/communication_models.md): socket-only,
  REST-only, and hybrid backends.
- [Hybrid races](docs/races.md): event/REST reconciliation policy.

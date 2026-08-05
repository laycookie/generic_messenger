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
let audio = messenger.clone().arc_audio().ok();
let edit = text.clone().arc_edit().ok();
```

Optional capability traits use `<ParentFeature><Capability>` names, such as
`TextDelete` for an operation requiring `Text`.

The borrowed caster (`audio()`, `edit()`, etc.) is useful for immediate method
calls. The `Arc` caster (`arc_audio()`, `arc_edit()`, etc.) supplies owned
trait objects for asynchronous tasks and methods whose receiver is
`self: Arc<Self>`, especially `listen`.

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
| `create_messenger(cred)` | Construct an authenticated backend instance. Returns `Arc<dyn Messenger>`. |
| `auth_methods()` | Declare login forms the UI can render. |
| `name()` | Return human-readable backend name. |
| `id()` | Return stable identity for this backend/account instance. Used for equality and registry keys. |
| `auth()` | Return credential subset safe and sufficient to persist and restore through `create_messenger`. |

Whole-feature casters:

| Caster trait | Borrowed method | Owned method |
| --- | --- | --- |
| `MessengerCasterQuery` | `query()` | `arc_query()` |
| `MessengerCasterText` | `text()` | `arc_text()` |
| `MessengerCasterAudio` | `audio()` | `arc_audio()` |
| `MessengerCasterVideo` | `video()` | `arc_video()` |

Each caster returns `MessengerError::NotImplemented` when concrete backend
does not implement that feature trait.

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
| `listen()` | Subscribe to query-related deltas such as created rooms and presence changes. | `NotImplemented` |

## `Text`

Text history and sending form the required core. Other mutations are optional
capability traits.

| Method | Purpose | Default |
| --- | --- | --- |
| `get_messages(location, before, ordering)` | Fetch messages, optionally before a cursor. `Ordering::Time` requests oldest-first output. | Required |
| `pickable_emojis(location)` | Fetch custom emoji available to compose in this room. Unicode emoji are not included. | `NotImplemented` |
| `pickable_stickers(location)` | Fetch stickers available to send in this room. | `NotImplemented` |
| `send_message(location, contents)` | Send message and return server-confirmed message with final ID. | Required |
| `listen()` | Subscribe to message and reaction deltas. Initial state/history still comes from `get_messages`. | `NotImplemented` |

### Text operation capabilities

| Trait | Caster methods | Trait methods |
| --- | --- | --- |
| `TextEdit` | `edit()`, `arc_edit()` | `edit_message(location, message, contents)` replaces content and returns server-confirmed message. |
| `TextDelete` | `delete()`, `arc_delete()` | `delete_message(location, message)` deletes a sent message. |
| `TextCancelSend` | `cancel()`, `arc_cancel()` | `cancel_window()` returns eligibility duration; `cancel_send(location, message)` retracts within that window. |
| `TextReactions` | `reactions()`, `arc_reactions()` | `add_reaction(...)` and `remove_reaction(...)` mutate current user's reaction. |

`TextCancelSend` differs from `TextDelete`: cancel is a short-lived
undo-send operation; delete follows normal backend permissions and lifetime.

## `Audio`

Audio call lifecycle and event subscription. PCM crosses dedicated audio
channels, never `VoiceEvent`.

| Method | Purpose | Default |
| --- | --- | --- |
| `connect(location)` | Request voice connection. Returns current non-connected `CallStatus`; final readiness arrives asynchronously as `VoiceEvent::CallStreamReady`. | Required |
| `disconnect(location)` | Leave voice location. Trait intentionally makes teardown best-effort and errorless. | Required |
| `listen()` | Subscribe once to backend-wide voice control events. | `NotImplemented` |

### How `Audio::listen` works

`listen` is normally started when backend is registered, before any call:

```text
Arc<dyn Audio>
└── Audio::listen()
    └── WeakSocketStream<VoiceEvent>
        ├── CallStatusUpdate
        ├── ParticipantJoined / ParticipantLeft
        └── CallStreamReady
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

`listen(self: Arc<Self>)` consumes one `Arc` handle so implementation can build
owned async state without borrowing backend. Clone handle first when caller
also needs to retain it.

### `AudioSelfVoiceState`

Optional signaling capability for own mute/deafen flags.

| Method | Purpose |
| --- | --- |
| `set_self_voice_state(location, state)` | Publish own voice flags. Application remains responsible for actually muting microphone/playback. |

`AudioCasterSelfState` exposes `self_state()` and `arc_self_state()`.
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
| `listen()` | Subscribe to stream lifecycle and frame-channel handshake events. | `NotImplemented` |

Video event flow:

- `VideoStreamAvailable`: remote stream exists but requires explicit
  `subscribe_video`.
- `AddVideoStream`: media delivery starts. Application creates
  `frame_channel()` and returns `FrameProducer`; application displays frames
  from `FrameConsumer`.
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

`Query::listen`, `Text::listen`, `Audio::listen`, and `Video::listen` return
`WeakSocketStream<Event>`, which implements `futures::Stream`.

On each poll, `WeakSocketStream`:

1. upgrades weak event-source reference;
2. calls `ArcStream::next(Arc<Self>)`;
3. yields returned event;
4. ends when source is gone or `next` returns `None`.

Weak reference prevents a forgotten UI subscription from keeping discarded
backend alive. Implementations with indefinitely-pending `next` calls must
wake and return `None` when owner shuts down; see `ArcStream` documentation.

Each feature stream is intended for one active consumer per backend instance.
Implementations may drain shared queues, so multiple listeners are not
guaranteed broadcast copies.

### `ArcStream`

Low-level trait implemented by backend event sources.

| Method | Purpose |
| --- | --- |
| `next(self: Arc<Self>)` | Await next item using shared ownership; return `None` to end stream. |

Most application code consumes `WeakSocketStream` through `StreamExt`, not
`ArcStream` directly.

## Implementing a backend

1. Implement `Messenger`.
2. Implement supported whole-feature traits (`Query`, `Text`, `Audio`,
   `Video`). Generated casters expose them automatically.
3. Implement optional operation traits only when support is real and usable.
4. Implement/retain an `ArcStream` event source for each supported `listen`.
5. Keep native IDs/codecs/protocol payloads inside adapter.
6. Return shared `Identifier`, event, audio-channel, and video-frame types at
   interface boundary.

## Design notes

- [Communication models](docs/communication_models.md): socket-only,
  REST-only, and hybrid backends.
- [Hybrid races](docs/races.md): event/REST reconciliation policy.

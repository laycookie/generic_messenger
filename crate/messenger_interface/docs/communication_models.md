# How messenger backends communicate

The `messenger_interface` abstraction has to fit several different shapes of
backend. Each shape comes with different assumptions about how state stays
in sync between client and server, and where races (or the absence of them)
live. This document sketches the three meaningful patterns and where each
sits on the tradeoff spectrum.

The reconciliation strategy chosen for hybrid backends is documented
separately in [`races.md`](./races.md); this document is about *why* such a
strategy is needed in the first place, by contrasting with the models that
don't need it.

## 1. Socket-only

The backend exposes a single bidirectional channel — websocket, raw TCP,
custom protocol — and every interaction flows through it. Requests go out
the same channel that events come back on. All state changes (created,
updated, deleted) arrive as ordered messages on one stream.

**Examples in the wild:** IRC, classic XMPP, some Matrix configurations
where the federated event stream is the only API, custom realtime systems.

**State synchronization is essentially free.** There is one source of truth,
ordered by the socket. The fetch-vs-event race doesn't exist because they're
the same channel: if a "fetch" is just a server message saying "here's the
current state of X," it slots into the event order naturally. Reconnection
needs replay or a snapshot opcode, but the client never has to reconcile
two views of the world.

**Implementation cost moves into the protocol.** Every operation needs an
event for it, and the backend has to handle back-pressure, replay, and
reconnect cleanly. The client side is much simpler — no merging, no
tombstones, no priority rules.

**Liability: latency tolerance.** A socket-only backend can do nothing while
the socket is closed. Mobile background, network blip, server restart all
become full stalls. There's no fallback path.

In the `messenger_interface` model, a socket-only backend implements
`Query` mostly trivially: each query waits for or reads from
cache-populated-from-events. The cache *is* the source of truth, no HTTP
fallback needed.

## 2. REST-only

The backend exposes a request/response API only. State changes are observed
by polling. There is no persistent realtime channel.

**Examples in the wild:** Email (poll inbox via IMAP/POP), batch-oriented
messaging, RSS-style notification systems, some enterprise chat platforms
without realtime. A hypothetical "shell-styled" backend — where the user
explicitly runs a command to pull state, like a REPL or CLI tool — also
falls in this category.

**State synchronization tradeoffs:**

- Freshness is bounded by the poll interval. Fast polling means high server
  load but fast notifications; slow polling means low load but stale state.
- Long-polling and Server-Sent Events sit in the middle ground but, from
  the client's perspective, are still effectively HTTP.
- The client must track "last seen" markers (cursors, timestamps, sequence
  numbers) to avoid reprocessing entries on each poll.

**The fetch-vs-event race doesn't apply** — there's no event channel to
race against the fetch. But other races do appear: two concurrent fetches
can return inconsistent views if the server doesn't expose a snapshot or
cursor mechanism, and the client has to deduplicate carefully across polls
if entries can change between them.

In the `messenger_interface` model, a REST-only backend implements `Query`
straightforwardly (each method is one or more HTTP calls) but `listen()`
has to be synthesized client-side as a polling loop. That polling loop
needs persistent "last seen" state to survive restarts, which pushes more
bookkeeping into the backend than a socket-only design needs.

**Whether this is viable for chat depends entirely on freshness
requirements.** For a Slack-like UX with sub-second message delivery,
REST-only is impractical — you'd be polling every backend, every user,
every channel, constantly. For a notification-light or async backend
(think email, or a CLI tool the user runs explicitly), it's reasonable and
dramatically simpler to implement.

The "shell-styled" idea — a messenger that the user explicitly invokes to
pull state, rather than one that pushes notifications — is actually a
*defensible* shape for some use cases (privacy-focused tools where always-on
network connections are undesirable; batch communication systems; tools
embedded in developer workflows where the user is already in a terminal).
It just isn't what most users expect from a chat client.

## 3. Hybrid: realtime channel plus REST

The backend exposes both a persistent socket for events *and* a REST API
for explicit operations. The two channels each have a natural domain:

- **Socket** delivers state-change events as they happen (a message
  arrived, a user joined a voice channel, a reaction was added).
- **REST** handles writes (send message, delete message), bulk fetches
  (message history older than the connection), and bootstrap (the initial
  state on connect).

**Examples in the wild:** Discord, Slack, modern Matrix clients (sync
endpoint + websocket optimizations), most "modern chat" platforms.

**This is where the races live.** The two channels are independent and
unsynchronized. The same change can appear via both channels with no
shared ordering between them. See [`races.md`](./races.md) for the
enumeration of consequences and the reconciliation policy chosen here.

**Why backends do it anyway:** the socket is efficient for the high-volume
case (a message arrived → a few hundred bytes of event), and REST is
efficient for the bootstrap case (here's your complete inbox) and the
modify case (POST a new message, DELETE this old one). Forcing
"send a message and observe the result" entirely over a socket requires
the protocol to grow its own RPC layer; forcing "100 new messages just
arrived" entirely over REST forces polling. Each channel handles what
it's good at, and the cost is reconciliation complexity at the client.

**Discord's specific choice** is to make the socket-side snapshot (the
`Ready` payload) comprehensive enough that REST is rarely needed after
bootstrap. This pushes most of the reconciliation problem to "the first
few seconds after connect" plus a small number of REST-only paths
(message history, writes). Defensible engineering — but the implementation
still has to handle these races correctly, and the `Ready` snapshot is
expensive enough that backends with smaller resource budgets often can't
follow the same approach.

## Implications for `messenger_interface`

The trait split into `Query`, `Text`, `Voice` deliberately stays agnostic
to which model a backend uses. A socket-only backend can implement `Query`
by reading its local cache; a REST-only backend can implement `listen()`
as a polling loop; a hybrid backend has to handle both paths.

The reconciliation policy described in `races.md` (cache is
gateway-authoritative; HTTP fills gaps; tombstones for deletes) is designed
to be safe across these models:

- **Socket-only**: HTTP is never called, so the tombstone path is dormant
  and harmless. The cache is the gateway-driven source of truth and that's
  all there is.
- **REST-only**: no events ever arrive, so the cache is purely
  HTTP-populated and the merge rule degenerates to "always take HTTP."
  Tombstones are never written.
- **Hybrid**: the full policy applies — gateway-priority cache, HTTP fills
  unknown IDs, tombstones guard against resurrection.

Preserving this generality matters as the abstraction grows. New backends
shouldn't have to reinvent reconciliation just because their communication
shape is different from the first one we implemented.

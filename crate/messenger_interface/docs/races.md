# Race conditions in hybrid messenger backends

This document captures the race conditions that arise when a backend exposes
*both* a stateful query API (REST) and a delta-driven event stream (gateway /
websocket). Discord is the concrete case worked through here, but the patterns
apply to any hybrid backend implemented behind the `Query` / `Text` / `Audio`
traits in `messenger_interface`.

For a wider discussion of backend communication shapes (socket-only,
REST-only, hybrid) and where these races even apply, see
[`communication_models.md`](./communication_models.md).

## The fundamental race

The `Query` trait lets the UI call fetch-style methods (`rooms`, `houses`,
`get_messages`, ...) and also subscribe via `listen()` to an event stream.
Both eventually surface to the UI as state-mutation calls. They are not
synchronized with each other.

A fetch and an event can race in three meaningful ways:

1. **Stale fetch / fresh event.** The HTTP response reflects the world at
   time `T_http`; a gateway event with effective time `T_event > T_http` is
   processed locally before the HTTP response lands. If the UI applies the
   HTTP result *after* the event, the event's change is silently reverted.
2. **Resurrected deletion.** A `MessageDelete` (or any `*_DELETE`) event
   arrives during an in-flight HTTP fetch. The UI processes the delete and
   removes the entity. The HTTP response lands later, still containing the
   now-deleted entity, and the UI re-adds it.
3. **Phantom update.** An entity edit (e.g., `MessageUpdate` with new
   content, `MessageReactionRemove` clearing a reaction) lands during an HTTP
   fetch. The HTTP response returns the pre-edit state. UI applies the
   post-edit state from the event, then the HTTP-returned pre-edit state
   silently overrides.

None of these matter when only one channel is active:

- If the gateway is closed, the HTTP snapshot is authoritative.
- If only the gateway is active and we never call HTTP, there's no race.

The race is a *consequence* of having both channels open. Socket-only and
REST-only backends sidestep it entirely (see `communication_models.md`).

## Why a local sequence number doesn't fully solve it

The instinct is: bump an atomic counter on every gateway-event-driven cache
write, tag fetches with the counter value they observed, and reject events
that are "older" than the fetch's value.

This *works* for **cache-backed queries** (queries that return locally-stored
state populated by gateway events). The counter is the version of the local
cache; the fetch reads cache and counter atomically (with careful release /
acquire ordering); events with `seq <= fetch_seq` are guaranteed to already
be reflected in the data the fetch returned.

It does **not** work for **HTTP-backed queries**. An HTTP response is a
snapshot of the backend's *server-side* state at request-processing time,
which has no relationship to the local counter. A local counter says "I have
observed this many events"; it does not say "the data the server just gave
me is consistent with that view."

Discord's gateway *does* expose sequence numbers (the `s` field on every
dispatch — already tracked as `last_sequence_number` in
`interface/discord/src/gateways/general.rs`), but they're for the **Opcode 6
Resume** path on disconnect, not for HTTP correlation. The REST API doesn't
echo seq numbers in responses, so there is no protocol-level handshake we
could use even if we wanted to.

## How Discord's own client sidesteps it

Discord's official client minimizes HTTP usage rather than reconciling HTTP
intelligently. The `Ready` payload is allowed to be 10MB precisely because
the client is expected to seed its entire local state from it. Combined with
delta events, HTTP becomes a fallback for things the gateway cannot deliver:
message history older than the connection, attachment uploads, writes.
"Gateway is the world."

This is workable for a single-backend client but doesn't generalize. The
`messenger_interface` abstraction has to support backends not shaped this
way, so we need an explicit reconciliation strategy rather than just "trust
the gateway."

## The chosen reconciliation policy

> **The local cache, populated by gateway events, is authoritative. HTTP
> only writes to cache entries the cache hasn't seen, and gateway state
> always wins for entries the cache *has* seen.**

This per-ID merge policy follows from a simple observation: gateway events
are scoped to specific entity IDs. They invalidate *only* the IDs they name.
HTTP entries for unreferenced IDs are still valid even after gateway events
have arrived. Merging at the ID level (rather than treating the entire HTTP
response as stale) is both correct and lossless.

The policy decomposes into three cases for each entity ID returned by an
HTTP fetch:

1. Cache has `Present(value)` for the ID → gateway wins, drop the HTTP entry.
2. Cache has `Deleted(id)` (tombstone) → drop the HTTP entry.
3. Cache has no entry → take the HTTP entry, insert as `Present`.

Case 2 is what makes the policy resilient to the resurrection race. Without
tombstones, a `MessageDelete` that lands before an HTTP response would
leave the cache with no record of the entity, the HTTP merge rule would
re-add it, and the deletion would be silently undone.

## The tombstone ring buffer

Tombstones only need to live as long as the race window. The relevant window
is "from when an HTTP request is fired to when its response is processed" —
typically sub-second. Anything Discord deleted longer ago than that would
also be reflected in Discord's HTTP view, so the tombstone is no longer
load-bearing.

Implementation: use a small lock-free atomic ring with overwrite-on-push. The
old workspace helper crate for this was removed while tombstone rings are not
wired into the app, so reintroduce this as a local helper only when the cache
policy actually needs it. For messages, the ring should live on
`gateways::general::General` so its lifetime matches the gateway connection's:

```rust
pub struct General {
    pub voice: VoiceGateway,
    pub deleted_message_ids: Ring<SNOWFLAKE, 100>,
}
```

This co-location is the load-bearing piece for the
"gateway-down implies REST-is-authoritative" property: when the gateway
disconnects, the whole `Gateway<General>` is dropped, taking the
tombstone ring with it. No `clear()` call, no risk of leftover state
filtering subsequent REST queries — the lifetime *is* the invariant.

`InnerDiscord::drop_tombstoned_messages` reflects this: it loads the
gateway and, if `None`, returns the HTTP response unfiltered.

The cap of **100** matches Discord's `MESSAGE_DELETE_BULK` per-event size,
so a single bulk-delete event fits exactly. Larger purges chain multiple
bulk events; older entries will be overwritten, but by the time that
happens (~100 newer deletes ago), Discord's HTTP view has near-certainly
caught up.

`Ring::push` is a single `fetch_add` on the head index plus an atomic
store into the resulting slot — no conditional eviction branch, the
modulo handles overflow. `Ring::contains` loads every slot atomically;
at this size it's cache-friendly and the order in which slots are
scanned doesn't matter. Slot value `T::default()` (here `0u64`) is the
never-written sentinel; real Discord snowflakes are non-zero, so this
can't collide with a live ID.

The same ring shape can be reused for future per-entity tombstone rings
(channels, guilds, relationships) — see "open issues" below.

**Ordering invariant in the event handler:** push to the tombstone ring
**before** pushing the corresponding `TextEvent::MessageDeleted` to the
consumer queue. The reverse order admits this race:

1. HTTP request in flight.
2. UI consumes `MessageDeleted` from the text stream, removes from view.
3. HTTP response lands, queue check passes (ID not yet there).
4. UI re-adds the deleted message.
5. Tombstone finally arrives in the ring — too late to help.

## Cache-backed queries: gateway is the only writer

The reconciliation policy above is stated in its fully general form, but most of
the `Query` / `Text` entry points in the Discord backend are actually a
strictly simpler shape: **REST is a cold-start fallback, and once the cache is
populated by the gateway, REST is never consulted again.** The race window for
these queries is correspondingly narrow.

Concretely, `houses()`, `rooms()`, and `house_details()` (in
`interface/discord/src/query.rs`) all follow the same pattern:

```rust
async fn houses(&self) -> Result<Vec<...>, ...> {
    if let Some(cached) = self.guilds.load_full() {
        return Ok(self.process_guilds(&cached).await);
    }
    let guilds = self.rest_get_guilds().await?;
    Ok(self.process_guilds(&guilds).await)
}
```

Two properties matter here, and they hold by inspection:

1. **Gateway dispatch is the sole cache writer.** `Ready` publishes the initial
   snapshot; guild/channel/relationship/expression create, update, and delete
   reducers keep it current. REST fallback returns data directly and never
   writes it into these caches.
2. **A fresh `Ready` invalidates the previous session first.** Reconnect clears
   every session-scoped slice and index, then publishes the new snapshot.
   Empty collections are published explicitly, so entities deleted while the
   connection was down cannot survive reconnect.

Together these mean that for cache-backed queries:

- **Steady state (post-`Ready`):** REST is never hit. There is no race surface.
  A `GUILD_UPDATE` event simply mutates the cache and emits a `QueryEvent`; the
  UI applies it; done. No HTTP response can arrive late and undo it because no
  HTTP request was ever in flight.
- **Cold-start window (gateway-up but pre-`Ready`):** REST may be in flight
  when `Ready` and subsequent gateway events land. Because REST does not write
  to the cache, there is no cache-clobber. The only residual race is at the UI
  layer: if the UI consumes a `QueryEvent` from the event stream **before** the
  in-flight REST future resolves, it can apply the update and then have it
  visually overwritten when the stale REST snapshot is rendered. This window
  is typically 1–3 seconds and ends permanently once `Ready` lands.

This narrower race surface is why the "Audit" table below distinguishes
*cache-backed* from *every-call HTTP*. Cache-backed fetches need
`*_UPDATE` / `*_DELETE` handlers for correctness (otherwise the cache goes
stale), but they do **not** need tombstone rings or per-ID merge policies for
the race story alone — those are required only for fetches whose results are
returned to the UI on every call without ever passing through the cache.

The fetches that **do** require the full reconciliation policy are
`rest_get_messages` (called on every `Text::get_messages`) and
`rest_get_contacts` (called on every `Query::contacts`). Those have no
cache-first path, so the race is open on every invocation, not just at
cold-start.

## Audit: which fetches are exposed?

| Fetch | Cache-backed? | Matching delete event | Status today | Latent risk |
|---|---|---|---|---|
| `rest_get_profile` | yes | (no DELETE for self) | safe | n/a |
| `rest_get_contacts` | yes (cold-start only) | `RELATIONSHIP_REMOVE` | cache reducer handled | cold-start UI window only |
| `rest_get_dms` | yes (cold-start only) | `CHANNEL_DELETE` (DM) | cache reducer handled | cold-start UI window only |
| `rest_get_guilds` | yes (cold-start only) | `GUILD_DELETE` | cache reducer handled | cold-start UI window only |
| `rest_get_guild_channels` | yes (cold-start only) | `CHANNEL_DELETE` (guild) | cache reducer handled | cold-start UI window only |
| `rest_get_messages` | **no, every call HTTP** | `MessageDelete`, `MessageDeleteBulk` | recorded and merged | creation/update/delete/reactions reconciled |

Cache-first fetches are only at risk during the gateway-connect-to-Ready
window (typically 1–3 seconds), and only once the corresponding `*_DELETE`
handlers are added. Defensible to defer tombstone rings for those entity
types until those handlers are wired up.

## Open issues for later

1. **Cold-start UI replacement.** Cache-backed queries can still have an
   already-consumed realtime event visually overwritten by the one REST
   fallback started before `Ready`. Fix belongs in the UI reducer: merge
   entities by ID instead of replacing whole lists.
2. **Partial message updates.** Discord can send embed-only partial
   `MESSAGE_UPDATE` payloads that omit author/content/timestamp. Current adapter
   intentionally skips fields it cannot represent. Extend wire model only when
   interface exposes those fields.

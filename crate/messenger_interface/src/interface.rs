//! Messenger abstraction layer.
//!
//! This module defines the traits that each messenger backend (Discord, Matrix, etc.)
//! must implement so the rest of the application can interact with it in a uniform way.
//! Feature areas live in submodules by concern (`query`, `text`, `voice`) and are
//! re-exported flat here, so consumers keep using `messenger_interface::interface::*`
//! regardless of which file a given item lives in.
//!
//! Design notes:
//! - Optional features come in two granularities, both decided per backend
//!   *type* at compile time via the `capability_caster!` specialization pair:
//!   whole feature areas ([`Query`], [`Text`], [`Voice`]) casted from
//!   [`Messenger`], and single operations ([`EditMessage`], [`DeleteMessage`],
//!   [`CancelSend`]) casted from [`Text`]. A backend that doesn't implement a
//!   capability has no call path for it, and the UI can hide the affordance
//!   with a cheap synchronous check — no probing call needed.
//! - Some remaining trait methods (on [`Query`], reactions/pickers on [`Text`])
//!   still use default bodies returning [`MessengerError::NotImplemented`];
//!   those are candidates for the same treatment if the UI ever needs to know
//!   about them without calling.
//! - Errors are intentionally trait-object based (`Box<dyn Error + Send + Sync>`) to
//!   allow each backend to return its own error types without exposing them here.
use std::fmt::Debug;
use std::{error::Error, sync::Arc};

use crate::auth::{AuthMethod, Credential};

pub use crate::stream::{ArcStream, WeakSocketStream};

use async_trait::async_trait;
use facet::Facet;

mod caster;
mod query;
mod text;
mod voice;

use caster::capability_caster;

pub use query::{Query, QueryCasterUserStatuses, QueryEvent, UserStatuses};
pub use text::{
    CancelSend, DeleteMessage, EditMessage, Ordering, Reactions, Text, TextCasterCancel,
    TextCasterDelete, TextCasterEdit, TextCasterReactions, TextEvent,
};
pub use voice::{
    AudioEvent, CallState, CallStatus, SelfVoiceState, Voice, VoiceCasterSelfState, VoiceEvent,
};

#[derive(Debug, Facet)]
#[facet(derive(Error))]
#[repr(u8)]
pub enum MessengerError {
    // #[facet(diagnostic::help = "Feature not implemented on this messenger")]
    NotImplemented,
    // #[facet(diagnostic::help = "Has some prerequest that wasnt fullfiled")]
    Requires,
}

capability_caster!(
    /// Capability: query (fetch) state from the messenger.
    MessengerCasterQuery: Messenger => Query { query, arc_query }
    /// Capability: text chat integration.
    MessengerCasterText: Messenger => Text { text, arc_text }
    /// Capability: voice chat integration.
    MessengerCasterVoice: Messenger => Voice { voice, arc_voice }
);

/// A concrete messenger backend.
///
/// Implement this for each integration (e.g. Discord, Matrix, etc).
///
/// Feature areas are split into smaller traits (`Query`, `Socket`, `VC`, ...). Backends
/// can expose a sub-API by returning `Ok(&impl Trait)` from the corresponding method,
/// or return [`MessengerError::NotImplemented`] if they don't support it.
#[async_trait]
pub trait Messenger: Send + Sync
where
    Self: MessengerCasterQuery + MessengerCasterText + MessengerCasterVoice,
{
    /// Construct a backend instance from a [`Credential`]. Async and fallible:
    /// variants that need no network build immediately (the body simply does not
    /// await), while an interactive credential login (e.g. Discord
    /// username/password) resolves its token here. A backend returns `Err` for a
    /// credential variant it does not support or a missing field.
    async fn create_messenger(
        cred: Credential,
    ) -> Result<Arc<dyn Messenger>, Box<dyn Error + Send + Sync>>
    where
        Self: Sized;
    /// The login methods this backend offers, declared so the login UI is
    /// generated rather than hard-coded per platform.
    fn auth_methods() -> &'static [AuthMethod]
    where
        Self: Sized;
    /// Human-readable backend name (e.g. `"discord"`).
    fn name(&self) -> &'static str;
    /// Stable unique id for this backend instance (e.g. includes account/server context).
    fn id(&self) -> String;
    /// The credential to persist for this instance — the subset the backend wants
    /// saved (e.g. Discord a [`Credential::Token`], Steam a [`Credential::Session`]).
    /// Re-hydrated via [`create_messenger`](Messenger::create_messenger).
    fn auth(&self) -> Credential;
}

impl PartialEq for dyn Messenger {
    fn eq(&self, other: &Self) -> bool {
        self.id() == other.id()
    }
}

//! Shared data model for the messenger interface.
//!
//! The model is split across submodules by concern and re-exported flat here,
//! so consumers keep using `messenger_interface::types::*` regardless of which
//! file a given item lives in.

mod cache;
mod details;
mod identifier;
mod message;
mod place;
mod rich_text;
mod user;

pub use cache::{
    CACHE_IMGS_DIR, CacheCategory, TEMP_FILE_SUFFIX, cache_dir, cache_img_dir,
    sweep_stale_temp_files,
};
pub use details::Details;
pub use identifier::{ID, Identifier};
pub use message::{Message, Reaction, Revision};
pub use place::{House, Place, Room, RoomCapabilities};
pub use rich_text::{Emoji, RichText, Span, Sticker, TextStyle};
pub use user::{User, UserStatus, VoiceState};

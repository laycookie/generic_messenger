use std::{
    borrow::Borrow,
    hash::{Hash, Hasher},
    ops::Deref,
    path::PathBuf,
};

#[derive(Debug, Clone)]
pub struct Usr {
    pub name: String,
    pub icon: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct MessageContents {
    pub text: String,
    pub reactions: Vec<Reaction>,
}
impl MessageContents {
    pub fn simple_text(text: &str) -> Self {
        Self {
            text: text.to_string(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone)]
pub struct Message {
    pub author: Identifier<Usr>,
    pub contents: MessageContents,
}
impl Deref for Message {
    type Target = MessageContents;

    fn deref(&self) -> &Self::Target {
        &self.contents
    }
}

#[derive(Debug, Clone)]
pub struct Reaction {
    pub emoji: char,
    pub count: u32,
}

#[derive(Debug, Clone)]
pub enum ChanType {
    Spacer,
    Text,
    Voice,
    TextAndVoice,
}
#[derive(Debug, Clone)]
pub struct Chan {
    pub chan_type: ChanType,
    pub name: String,
    pub icon: Option<PathBuf>,
    pub participants: Vec<Identifier<Usr>>,
}

#[derive(Debug, Clone)]
pub struct Server {
    pub name: String,
    pub icon: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub enum CallStatus {
    Connected,
    Connecting,
    Disconnected,
}

pub type ID = u32;
#[derive(Debug, Clone)]
#[repr(C)]
pub struct Identifier<D> {
    pub id: ID,
    pub data: D,
}
impl<D> Identifier<D> {
    pub fn get_id(&self) -> &ID {
        &self.id
    }
    pub fn remove_data(self) -> Identifier<()> {
        Identifier {
            id: self.id,
            data: (),
        }
    }
}
impl<D> Deref for Identifier<D> {
    type Target = D;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl<D, E> PartialEq<Identifier<E>> for Identifier<D> {
    fn eq(&self, other: &Identifier<E>) -> bool {
        self.get_id() == other.get_id()
    }
}
impl<D> Eq for Identifier<D> {}

// TODO: Review this later
impl<D> Hash for Identifier<D> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.get_id().hash(state);
    }
}

impl Borrow<Identifier<()>> for Identifier<Chan> {
    fn borrow(&self) -> &Identifier<()> {
        unsafe { &*(self as *const Identifier<Chan> as *const Identifier<()>) }
    }
}

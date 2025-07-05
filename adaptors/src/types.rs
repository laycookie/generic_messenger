use std::{
    borrow::Borrow,
    hash::{Hash, Hasher},
    path::PathBuf,
};

#[derive(Debug, Clone)]
pub struct Server {
    pub name: String,
    pub icon: Option<PathBuf>,
}
#[derive(Debug, Clone)]
pub struct Chan {
    pub name: String,
    pub icon: Option<PathBuf>,
    pub particepents: Vec<Identifier<Usr>>,
}
#[derive(Debug, Clone)]
pub struct Usr {
    pub name: String,
    pub icon: Option<PathBuf>,
}
#[derive(Debug, Clone)]
pub struct Msg {
    pub author: Identifier<Usr>,
    pub text: String,
}

#[derive(Debug, Clone)]
#[repr(C)]
pub struct Identifier<D> {
    pub(crate) id: String,           // ID of a location inside the parent obj
    pub(crate) hash: Option<String>, // Used in cases where ID can change
    pub data: D,
}
impl<D> Identifier<D> {
    pub fn remove_data(self) -> Identifier<()> {
        Identifier {
            id: self.id.clone(),
            hash: self.hash.clone(),
            data: (),
        }
    }
}

impl<D, E> PartialEq<Identifier<E>> for Identifier<D> {
    fn eq(&self, other: &Identifier<E>) -> bool {
        self.id == other.id && self.hash == other.hash
    }
}
impl<D> Eq for Identifier<D> {}

// TODO: Review this later
impl<D> Hash for Identifier<D> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        self.hash.hash(state);
    }
}

impl Borrow<Identifier<()>> for Identifier<Chan> {
    fn borrow(&self) -> &Identifier<()> {
        unsafe { &*(self as *const Identifier<Chan> as *const Identifier<()>) }
    }
}

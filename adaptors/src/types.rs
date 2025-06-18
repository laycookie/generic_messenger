use std::path::PathBuf;

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
struct NewMsg {
    chan_id: Identifier<()>, // Needed to update the channel, so just ID is enough
    author: Identifier<Usr>,
    text: String,
}

#[derive(Debug, Clone)]
pub struct Identifier<D> {
    pub(crate) id: String,           // ID of a location inside the parent obj
    pub(crate) hash: Option<String>, // Used in cases where ID can change
    pub data: D,
}

use adaptors::{discord::Discord, Messanger as Auth};
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    path::PathBuf,
    str::FromStr,
    sync::Arc,
};

use crate::pages::login::Platform;

// TODO: Check why this req. pub
#[derive(Clone)]
pub(crate) struct Messanger {
    pub(crate) auth: Arc<dyn Auth>,
    save_to_disk: bool,
}

pub(super) struct AuthStore {
    messangers: Vec<Messanger>,
    file: File,
}

impl<'a> AuthStore {
    pub fn new(path: PathBuf) -> Self {
        let auth_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
            .expect(format!("{:#?}", path).as_str());

        let buf_reader = BufReader::new(&auth_file);

        let mut messangers = Vec::new();
        for auth_line in buf_reader.lines() {
            let auth_line = auth_line.unwrap(); // For now we don't handle this

            let (platform, token) = match auth_line.split_once(":") {
                Some(auth_data) => auth_data,
                None => continue,
            };

            // In theory should never return false
            let auth: Arc<dyn Auth> = match Platform::from_str(platform).unwrap() {
                Platform::Discord => Discord::new(token),
                Platform::Test => todo!(),
            };

            messangers.push(Messanger {
                auth,
                save_to_disk: true,
            });
        }
        AuthStore {
            file: auth_file,
            messangers,
            // auth_change_listeners: Vec::new(),
        }
    }

    fn sync_disk(&mut self) {
        // Preferably I should just be writing to a new file, and then
        // just swap the files when I'm finished writing, but realistically
        // there is no point in this type of redundancy at this point in the
        // project.
        self.file.seek(SeekFrom::Start(0)).unwrap();
        self.file.set_len(0).unwrap();
        self.messangers.iter_mut().for_each(|messangers| {
            if messangers.save_to_disk == false {
                return;
            }

            let auth = messangers.auth.as_ref();
            writeln!(self.file, "{}:{}", auth.name(), auth.id()).unwrap();
        });
    }

    pub fn is_empty(&self) -> bool {
        self.messangers.is_empty()
    }

    fn contains_auth(&self, auth: &Arc<dyn Auth>) -> bool {
        for i in self.get_messangers() {
            if &i.auth == auth {
                return true;
            }
        }
        false
    }

    pub fn get_auths(&self) -> Vec<Arc<dyn Auth>> {
        self.messangers
            .iter()
            .map(|m| m.auth.to_owned())
            .collect::<Vec<_>>()
    }
    pub fn get_messangers(&self) -> &[Messanger] {
        &self.messangers[..]
    }

    pub fn add_auth(&mut self, auth: Arc<dyn Auth>) -> bool {
        if !self.contains_auth(&auth) {
            self.messangers.push(Messanger {
                auth,
                save_to_disk: false,
            });
            // self.dispatch_callbacks();
            return true;
        }
        false
    }

    /// Saves everything to disk
    pub fn save_to_disk(&mut self) {
        for m in &mut self.messangers {
            if m.save_to_disk == false {
                m.save_to_disk = true;
            }
            println!("{:#?}", m.save_to_disk);
        }
        self.sync_disk();
    }
}

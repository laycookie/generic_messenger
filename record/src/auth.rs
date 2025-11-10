use adaptors::{Messanger as Auth, discord::Discord};
use std::{
    fs::OpenOptions,
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    path::PathBuf,
    str::FromStr,
    sync::Arc,
};

use crate::{messanger_unifier::Messangers, pages::login::Platform};

pub struct MessangersGenerator;
impl MessangersGenerator {
    pub fn messengers_from_file(path: PathBuf) -> Result<Messangers, Box<dyn std::error::Error>> {
        let auth_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;

        let buf_reader = BufReader::new(&auth_file);

        let mut messangers = Messangers::default();
        for auth_line in buf_reader.lines() {
            let auth_line = auth_line.unwrap(); // For now we don't handle this

            let (platform, token) = match auth_line.split_once(":") {
                Some(auth_data) => auth_data,
                None => continue,
            };

            // In theory should never return false
            let auth: Arc<dyn Auth> = Arc::new(match Platform::from_str(platform).unwrap() {
                Platform::Discord => Discord::new(token),
                Platform::Test => todo!(),
            });

            messangers.add_messanger(auth);
        }
        Ok(messangers)
    }
    pub fn messangers_to_file(messangers: &Messangers, path: PathBuf) {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();

        // Preferably I should just be writing to a new file, and then
        // just swap the files when I'm finished writing, but realistically
        // there is no point in this type of redundancy at this point in the
        // project.
        file.seek(SeekFrom::Start(0)).unwrap();
        file.set_len(0).unwrap();
        messangers.interface_iter().for_each(|messanger| {
            // if messanger.pressistent == false {
            //     return;
            // }

            writeln!(file, "{}:{}", messanger.name(), messanger.auth()).unwrap();
        });
    }
}

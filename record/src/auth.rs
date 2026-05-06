use std::{
    fs::OpenOptions,
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    path::PathBuf,
    str::FromStr,
};

use crate::{messenger_unifier::Messengers, pages::login::Platform};

pub struct MessengersGenerator;
impl MessengersGenerator {
    pub fn messengers_from_file(path: PathBuf) -> Result<Messengers, Box<dyn std::error::Error>> {
        let auth_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;

        let buf_reader = BufReader::new(&auth_file);

        let mut messengers = Messengers::default();
        for auth_line in buf_reader.lines() {
            let auth_line = auth_line.unwrap(); // For now we don't handle this

            let (platform, token) = match auth_line.split_once(":") {
                Some(auth_data) => auth_data,
                None => continue,
            };

            // In theory should never return false
            let auth = Platform::from_str(platform).unwrap().to_messenger(token);

            messengers.add_messenger(auth);
        }
        Ok(messengers)
    }
    pub fn messengers_to_file(messengers: &Messengers, path: PathBuf) {
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
        messengers.interface_iter().for_each(|messenger| {
            // if messenger.persistent == false {
            //     return;
            // }

            writeln!(file, "{}:{}", messenger.name(), messenger.auth()).unwrap();
        });
    }
}

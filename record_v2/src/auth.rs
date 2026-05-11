use std::{
    fs::OpenOptions,
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    path::PathBuf,
    str::FromStr,
};

use crate::{pages::login::Platform, state::MessengerRegistry};

pub struct MessengersGenerator;

impl MessengersGenerator {
    pub fn load(
        path: PathBuf,
        registry: &mut MessengerRegistry,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let auth_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;

        let buf_reader = BufReader::new(&auth_file);
        let mut loaded = false;

        for auth_line in buf_reader.lines() {
            let auth_line = auth_line?;

            let (platform, token) = match auth_line.split_once(':') {
                Some(auth_data) => auth_data,
                None => continue,
            };

            let auth = Platform::from_str(platform)?.to_messenger(token);
            registry.add(auth);
            loaded = true;
        }

        Ok(loaded)
    }

    pub fn save(registry: &MessengerRegistry, path: PathBuf) {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();

        file.seek(SeekFrom::Start(0)).unwrap();
        file.set_len(0).unwrap();

        for (_, entry) in registry.iter() {
            writeln!(file, "{}:{}", entry.interface.name(), entry.interface.auth()).unwrap();
        }
    }
}

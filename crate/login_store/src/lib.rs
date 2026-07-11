//! Persistence for the `LoginInfo` credential file.
//!
//! The file format is one `name:auth` pair per line, where `name` identifies a
//! platform and `auth` is that platform's serialized [`Credential`]. The generic
//! [`load`]/[`save`] functions own the line layer (the `name:auth` split); the
//! [`to_line`]/[`from_line`] codec owns turning a [`Credential`]'s collected
//! fields into a delimiter-safe `auth` blob and back.
//!
//! A blob is a percent-encoded `key=value&...` list, one pair per collected
//! field (the key is the field's schema key), so a secret containing `:` `&` `=`
//! or a newline round-trips intact. On load, each key is resolved back to one of
//! the backend's known field keys.

use std::{
    fs::OpenOptions,
    io::{self, BufRead, BufReader, Seek, SeekFrom, Write},
    path::Path,
};

use messenger_interface::auth::{Credential, CredentialField};

/// Serialize a [`Credential`] to a single delimiter-safe `auth` blob (see module
/// docs): one percent-encoded `key=value` pair per field.
pub fn to_line(credential: &Credential) -> String {
    let mut line = String::new();
    for field in &credential.0 {
        if !line.is_empty() {
            line.push('&');
        }
        line.push_str(&encode(field.key));
        line.push('=');
        line.push_str(&encode(field.value.unsecure()));
    }
    line
}

/// Parse a blob produced by [`to_line`], resolving each stored key to one of the
/// schema's `keys` (the backend's known field keys, so the parsed
/// [`CredentialField`] can hold a `&'static str`). Returns `None` if any pair is
/// malformed (no `=`, or an invalid percent escape) or carries a key not in
/// `keys` — an unrecognized field rejects the whole credential rather than being
/// silently dropped.
///
/// Also accepts the short-lived migration shape where the whole `key=value&...`
/// blob was percent-encoded as one string, so `token=...` became `token%3D...`.
pub fn from_line(line: &str, keys: &[&'static str]) -> Option<Credential> {
    parse_fields(line, keys).or_else(|| {
        let decoded = decode(line)?;
        parse_fields(&decoded, keys)
    })
}

fn parse_fields(line: &str, keys: &[&'static str]) -> Option<Credential> {
    let mut fields = Vec::new();
    for pair in line.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=')?;
        let key = decode(key)?;
        let key = keys.iter().copied().find(|known| *known == key)?;
        fields.push(CredentialField::new(key, decode(value)?));
    }
    Some(Credential(fields))
}

/// Percent-encode every byte outside the unreserved set, so the result is safe to
/// place in a `key=value&...` blob (no `:` `&` `=` `%` or newline survive
/// unescaped).
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

/// Reverse of [`encode`]. Returns `None` on a malformed `%XX` sequence or a
/// non-UTF-8 result.
fn decode(value: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut iter = value.bytes();
    while let Some(byte) = iter.next() {
        if byte == b'%' {
            let hex = |c: u8| match c {
                b'0'..=b'9' => Some(c - b'0'),
                b'a'..=b'f' => Some(c - b'a' + 10),
                b'A'..=b'F' => Some(c - b'A' + 10),
                _ => None,
            };
            bytes.push((hex(iter.next()?)? << 4) | hex(iter.next()?)?);
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).ok()
}

/// Reads `path`, mapping each `name:auth` line through `resolve`.
///
/// The file is created if it does not exist (yielding an empty `Vec`). Lines
/// without a `:` separator, and lines whose `resolve` returns `None` (e.g. an
/// unrecognized platform), are skipped — a single bad line does not abort the
/// load.
pub fn load<T>(
    path: impl AsRef<Path>,
    mut resolve: impl FnMut(&str, &str) -> Option<T>,
) -> io::Result<Vec<T>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)?;

    let mut loaded = Vec::new();

    for line in BufReader::new(&file).lines() {
        let line = line?;

        let Some((name, auth)) = line.split_once(':') else {
            continue;
        };

        if let Some(value) = resolve(name, auth) {
            loaded.push(value);
        }
    }

    Ok(loaded)
}

/// Truncates `path` and writes one `name:auth` line per entry.
pub fn save<I, K, V>(path: impl AsRef<Path>, entries: I) -> io::Result<()>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;

    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;

    for (name, auth) in entries {
        writeln!(file, "{}:{}", name.as_ref(), auth.as_ref())?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Look up a field's value by key in a parsed credential.
    fn value<'a>(cred: &'a Credential, key: &str) -> Option<&'a str> {
        cred.0
            .iter()
            .find(|field| field.key == key)
            .map(|field| field.value.unsecure())
    }

    #[test]
    fn value_round_trips_through_special_chars() {
        let cred = Credential(vec![CredentialField::new("token", "abc:123&x=%y zπ")]);
        let line = to_line(&cred);
        // The blob must be free of the delimiters the file format uses.
        assert!(!line.contains(['\n', ':', ' ']));
        let restored = from_line(&line, &["token"]).unwrap();
        assert_eq!(value(&restored, "token"), Some("abc:123&x=%y zπ"));
    }

    #[test]
    fn multiple_fields_round_trip_including_keys() {
        let cred = Credential(vec![
            // Keys are encoded too, so a key with a delimiter survives.
            CredentialField::new("user:name&", "alice"),
            CredentialField::new("token", "t=%ok"),
        ]);
        let keys = ["user:name&", "token"];
        let restored = from_line(&to_line(&cred), &keys).unwrap();
        assert_eq!(value(&restored, "user:name&"), Some("alice"));
        assert_eq!(value(&restored, "token"), Some("t=%ok"));
        assert_eq!(value(&restored, "missing"), None);
    }

    #[test]
    fn fully_encoded_blob_is_accepted_for_migration() {
        let restored = from_line("token%3Dabc.def%253Aghi", &["token"]).unwrap();
        assert_eq!(value(&restored, "token"), Some("abc.def:ghi"));
    }

    #[test]
    fn malformed_pairs_are_rejected() {
        assert!(from_line("no-equals", &["token"]).is_none());
        assert!(from_line("token=%ZZ", &["token"]).is_none());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        // A stored key the schema doesn't declare rejects the whole credential.
        assert!(from_line("ghost=value", &["token"]).is_none());
    }
}

//! Delimiter-safe credential blob encoding and decoding.

use messenger_interface::auth::{Credential, CredentialField};
use zeroize::Zeroizing;

/// Serialize a [`Credential`] to a single delimiter-safe `auth` blob (see crate
/// docs): one percent-encoded `key=value` pair per field.
pub fn to_line(credential: &Credential) -> String {
    let mut line = String::new();
    for field in &credential.0 {
        if !line.is_empty() {
            line.push('&');
        }
        encode_into(&mut line, field.key);
        line.push('=');
        encode_into(&mut line, field.value.unsecure());
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
        let decoded = Zeroizing::new(decode(line)?);
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
fn encode_into(out: &mut String, value: &str) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[usize::from(byte >> 4)] as char);
            out.push(HEX[usize::from(byte & 0x0f)] as char);
        }
    }
}

pub(crate) fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    encode_into(&mut out, value);
    out
}

/// Reverse of [`encode`]. Returns `None` on a malformed `%XX` sequence or a
/// non-UTF-8 result.
pub(crate) fn decode(value: &str) -> Option<String> {
    let mut bytes = Zeroizing::new(Vec::with_capacity(value.len()));
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
    match String::from_utf8(std::mem::take(&mut *bytes)) {
        Ok(decoded) => Some(decoded),
        Err(error) => {
            let _invalid = Zeroizing::new(error.into_bytes());
            None
        }
    }
}

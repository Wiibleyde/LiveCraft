//! Stream keys and passwords in the session file, sealed with the account:
//! another account, or a copy of the file, cannot read them. How depends on
//! the system, see `platform`.

const MARK: &str = "sealed:";

pub fn is_sealed(text: &str) -> bool {
    text.starts_with(MARK)
}

pub(crate) fn base64(bytes: &[u8]) -> String {
    const LETTERS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().fold(0u32, |a, b| a << 8 | *b as u32) << (8 * (3 - chunk.len()));
        for k in 0..4 {
            if k <= chunk.len() {
                out.push(LETTERS[(n >> (18 - 6 * k) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Empty text stays empty. If sealing fails the text stays as it was: a key
/// that is kept beats a key that is lost.
pub fn seal(text: &str) -> String {
    if text.is_empty() || is_sealed(text) {
        return text.to_string();
    }
    match crate::platform::seal(text.as_bytes()) {
        Some(sealed) => format!("{MARK}{}", base64(&sealed)),
        None => text.to_string(),
    }
}

/// Text that was never sealed comes back as it is. Text sealed by another
/// account comes back empty, to be typed again.
pub fn open(text: &str) -> String {
    let Some(coded) = text.strip_prefix(MARK) else { return text.to_string() };
    let bytes = crate::api::base64_decode(coded);
    crate::platform::open_sealed(&bytes)
        .map_or_else(String::new, |plain| String::from_utf8_lossy(&plain).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_survives_the_round_trip_and_is_not_readable() {
        let sealed = seal("clé-de-diffusion");
        assert!(is_sealed(&sealed));
        assert!(!sealed.contains("diffusion"));
        assert_eq!(open(&sealed), "clé-de-diffusion");
        assert_eq!(seal(&sealed), sealed, "sealing twice changes nothing");
    }

    #[test]
    fn plain_and_empty_text_pass_through() {
        assert_eq!(seal(""), "");
        assert_eq!(open("ancienne clé"), "ancienne clé");
        assert_eq!(open("sealed:AAAA"), "", "unreadable, never garbage");
    }

    #[test]
    fn base64_matches_the_decoder() {
        for text in ["a", "ab", "abc", "abcd"] {
            assert_eq!(crate::api::base64_decode(&base64(text.as_bytes())), text.as_bytes());
        }
        assert_eq!(base64(b"user:secret"), "dXNlcjpzZWNyZXQ=");
    }
}

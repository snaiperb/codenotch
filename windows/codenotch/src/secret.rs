//! At-rest protection for the admin keys in config.json.
//!
//! Windows DPAPI (`CryptProtectData`, user scope): the blob can only be decrypted by the same
//! Windows account on the same machine, so a copied config.json — or a backup that ends up
//! somewhere else — carries nothing usable. The file holds `dpapi:<hex>`; anything without that
//! prefix is treated as plaintext (a hand-edited value, or a config from before this landed) and
//! is re-written protected on the next save. Keys never leave the process in the protected form:
//! the settings window and the providers always see plaintext.

const PREFIX: &str = "dpapi:";

pub fn protect(plain: &str) -> String {
    if plain.is_empty() {
        return String::new();
    }
    match dpapi(plain.as_bytes(), true) {
        Some(blob) => format!("{PREFIX}{}", hex(&blob)),
        None => plain.to_string(), // DPAPI unavailable: keep working, keep plaintext (as before)
    }
}

pub fn unprotect(stored: &str) -> String {
    let Some(h) = stored.strip_prefix(PREFIX) else { return stored.to_string() };
    let Some(blob) = unhex(h) else { return String::new() };
    match dpapi(&blob, false) {
        Some(plain) => String::from_utf8(plain).unwrap_or_default(),
        None => String::new(), // another user or machine: the key is simply gone, never garbage
    }
}

pub fn is_protected(stored: &str) -> bool {
    stored.starts_with(PREFIX)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(windows)]
fn dpapi(input: &[u8], encrypt: bool) -> Option<Vec<u8>> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB};
    let mut inp = CRYPT_INTEGER_BLOB { cbData: input.len() as u32, pbData: input.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB::default();
    let ok = unsafe {
        if encrypt {
            CryptProtectData(&mut inp, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        } else {
            CryptUnprotectData(&mut inp, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        }
    };
    if ok.is_err() || out.pbData.is_null() {
        return None;
    }
    let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() };
    unsafe {
        let _ = LocalFree(HLOCAL(out.pbData as *mut core::ffi::c_void));
    }
    Some(v)
}

#[cfg(not(windows))]
fn dpapi(_input: &[u8], _encrypt: bool) -> Option<Vec<u8>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip_and_plaintext_passthrough() {
        assert_eq!(unhex(&hex(b"abc\x00\xff")).unwrap(), b"abc\x00\xff");
        assert_eq!(unhex("abc"), None);
        assert_eq!(unprotect("sk-plain"), "sk-plain");
        assert_eq!(protect(""), "");
        assert!(!is_protected("sk-plain"));
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_roundtrip() {
        let p = protect("sk-ant-admin01-secret");
        assert!(is_protected(&p));
        assert_ne!(p, "sk-ant-admin01-secret");
        assert_eq!(unprotect(&p), "sk-ant-admin01-secret");
        assert_eq!(unprotect("dpapi:zz"), "");
    }
}

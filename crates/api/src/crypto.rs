//! Request encryption used by the NetEase endpoints YoruPlay relies on.
//!
//! Ported from `util/crypto.js` in api-enhanced (MIT, commit recorded in
//! `docs/upstream-api-map.md`).  The Web API uses AES-128-CBC twice plus raw
//! RSA; the client API ("eapi") uses AES-128-ECB with an MD5 digest binding the
//! URL to the payload.
//!
//! No response encryption is enabled by this client, so only encryption is
//! implemented.  That is intentional: `encryptResponse` remains false upstream
//! and the endpoints return plain JSON.

use aes::Aes128;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use md5::{Digest, Md5};
use num_bigint::BigUint;
use rand::Rng;

const IV: &[u8; 16] = b"0102030405060708";
const PRESET_KEY: &[u8; 16] = b"0CoJUm6Qyw8W8jud";
const EAPI_KEY: &[u8; 16] = b"e82ckenh8dichen8";
const BASE62: &[u8; 62] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
const RSA_MODULUS_HEX: &str = "e0b509f6259df8642dbc35662901477df22677ec152b5ff68ace615bb7b725152b3ab17a876aea8a5aa76d2e417629ec4ee341f56135fccf695280104e0312ecbda92557c93870114af6c9d05c4f7f0c3685b7a46bee255932575cce10b424d813cfe4875d3e82047b97ddef52741d546b8e289dc6935b3ece0462db0a22b8e7";
const RSA_EXPONENT: u32 = 65537;
const RSA_BYTES: usize = 128;

type Aes128CbcEnc = cbc::Encryptor<Aes128>;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("invalid hexadecimal input: {0}")]
    Hex(String),
    #[error("RSA plaintext is too large")]
    RsaInputTooLarge,
    #[error("encryption failed")]
    Encryption,
}

fn aes_cbc_base64(text: &str, key: &[u8], iv: &[u8; 16]) -> String {
    let cipher = Aes128CbcEnc::new(key.into(), iv.into());
    let mut buf = vec![0u8; text.len() + 16];
    buf[..text.len()].copy_from_slice(text.as_bytes());
    let encrypted = cipher
        .encrypt_padded_mut::<Pkcs7>(&mut buf, text.len())
        .expect("buffer has room for one padding block");
    BASE64.encode(encrypted)
}

fn aes_ecb_hex_upper(text: &str, key: &[u8]) -> String {
    use aes::cipher::{BlockEncrypt, KeyInit};
    let cipher = Aes128::new(key.into());
    let mut data = text.as_bytes().to_vec();
    let block_size = 16;
    let padding = block_size - (data.len() % block_size);
    data.extend(std::iter::repeat_n(padding as u8, padding));
    for chunk in data.chunks_exact_mut(block_size) {
        let block = aes::Block::from_mut_slice(chunk);
        cipher.encrypt_block(block);
    }
    hex_upper(&data)
}

fn hex_upper(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02X}");
    }
    out
}

/// Raw RSA (no padding), matching node-forge's `encrypt(str, 'NONE')`.
/// The plaintext is interpreted as a big-endian unsigned integer.
fn rsa_encrypt_raw(plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if plaintext.len() > RSA_BYTES {
        return Err(CryptoError::RsaInputTooLarge);
    }
    let n = BigUint::parse_bytes(RSA_MODULUS_HEX.as_bytes(), 16)
        .ok_or_else(|| CryptoError::Hex("RSA modulus".into()))?;
    let m = BigUint::from_bytes_be(plaintext);
    let c = m.modpow(&BigUint::from(RSA_EXPONENT), &n);
    let bytes = c.to_bytes_be();
    if bytes.len() > RSA_BYTES {
        return Err(CryptoError::RsaInputTooLarge);
    }
    let mut out = vec![0u8; RSA_BYTES - bytes.len()];
    out.extend_from_slice(&bytes);
    Ok(out)
}

/// Encrypt a Web API payload. `secret_key` must be 16 ASCII bytes; callers
/// normally use [`random_weapi_secret`]. Exposed separately so tests can pin a
/// deterministic vector.
pub fn weapi_encrypt(text: &str, secret_key: &str) -> Result<(String, String), CryptoError> {
    if secret_key.len() != 16 || !secret_key.is_ascii() {
        return Err(CryptoError::Encryption);
    }
    let first = aes_cbc_base64(text, PRESET_KEY, IV);
    let params = aes_cbc_base64(&first, secret_key.as_bytes(), IV);
    let reversed: Vec<u8> = secret_key.bytes().rev().collect();
    let enc_sec_key = hex_lower(&rsa_encrypt_raw(&reversed)?);
    Ok((params, enc_sec_key))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

pub fn random_weapi_secret() -> String {
    let mut rng = rand::thread_rng();
    (0..16)
        .map(|_| BASE62[rng.gen_range(0..BASE62.len())] as char)
        .collect()
}

/// Encrypt an eapi payload. `path` must include the `/api/...` prefix, exactly
/// as the upstream module passed it.
pub fn eapi_encrypt(path: &str, text: &str) -> String {
    let message = format!("nobody{path}use{text}md5forencrypt");
    let digest = Md5::digest(message.as_bytes());
    let digest_hex = hex_lower(&digest);
    let data = format!("{path}-36cd479b6b5-{text}-36cd479b6b5-{digest_hex}");
    aes_ecb_hex_upper(&data, EAPI_KEY)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Generated against the upstream implementation (Node's crypto) as a
    // development-time reference.  These pin the protocol, not random output.
    #[test]
    fn weapi_matches_reference_vector() {
        let (params, sec) = weapi_encrypt("{\"type\":3}", "0123456789abcdef").unwrap();
        assert_eq!(params, "UrX63R19WuqrzE6+nEgz1aUzJYXbECu0SH1CcCGpaPY=");
        assert_eq!(
            sec,
            "35701388baf89fed412e11269b9c76625d095ecaf17f03fa018abe19ea2d38b949debf242ee39a71ca1f6cda71b1b86a45aa909ee27f7e78e267d34e732f0de948206c3340a788d0003372183e2f753c1f78b66ac23d134ac1fc9b993156520ea826b8aa89a962d4491b4b8d7e08738e1da9b07aa39bf4a7ef0b1c210728cd52"
        );
    }

    #[test]
    fn eapi_matches_reference_vector() {
        let encrypted = eapi_encrypt("/api/login/qrcode/unikey", "{\"type\":3}");
        assert_eq!(
            encrypted,
            "1CA81A97B7BAEB099F29A9B99A25CD6E58E7BA5B35F25231033065C776830C0A38A128CF8B3EAF44635E50E0B9800A4A037CBDA1E04BD460C02768E3ECD5E66E905A5BCAE52009FB0933EFB373A62E834048DEBDABCCC627F25820A8D94E0A8C"
        );
    }

    #[test]
    fn random_secret_is_base62() {
        for _ in 0..20 {
            let s = random_weapi_secret();
            assert_eq!(s.len(), 16);
            assert!(s.bytes().all(|b| BASE62.contains(&b)));
        }
    }

    #[test]
    fn rejects_bad_secret() {
        assert!(weapi_encrypt("{}", "short").is_err());
        assert!(weapi_encrypt("{}", "0123456789abcdeé").is_err());
    }
}

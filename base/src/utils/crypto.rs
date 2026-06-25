use aes::Aes256;
use aes_gcm::aead::consts::U12;
use aes_gcm::aead::{Aead, AeadCore, OsRng};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::Engine;
use block_modes::block_padding::Pkcs7;
use block_modes::{BlockMode, Cbc};
use log::error;
use rand::seq::SliceRandom;
use sha2::{Digest, Sha256};

use exception::{GlobalResult, GlobalResultExt};

type AesCbc = Cbc<Aes256, Pkcs7>;

const BASE_STR: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
const DEFAULT_KEY: &str = "1234567890All in Rust 1234567890"; //32长度

/// AES-256-GCM field cipher for recoverable secrets stored outside process memory.
///
/// The master key must be a 32-byte value encoded as base64 without padding.
/// Ciphertext format is version byte + 12-byte nonce + AES-GCM ciphertext/tag,
/// encoded as base64 without padding.
#[derive(Clone)]
pub struct Aes256GcmCipher {
    cipher: Aes256Gcm,
}

impl Aes256GcmCipher {
    pub fn from_base64_key_no_pad(encoded: &str) -> GlobalResult<Self> {
        let key = base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(encoded)
            .hand_log(|err| error!("{err}"))?;
        if key.len() != 32 {
            return Err(exception::GlobalError::new_sys_error(
                "AES-256-GCM key must decode to 32 bytes",
                |err| error!("{err}"),
            ));
        }
        let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| {
            exception::GlobalError::new_sys_error("invalid AES-256-GCM key", |err| error!("{err}"))
        })?;
        Ok(Self { cipher })
    }

    pub fn encrypt_to_base64_no_pad(&self, plaintext: &str) -> GlobalResult<String> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|_| {
                exception::GlobalError::new_sys_error("AES-256-GCM encrypt failed", |err| {
                    error!("{err}")
                })
            })?;
        let mut payload = Vec::with_capacity(1 + nonce.len() + ciphertext.len());
        payload.push(1);
        payload.extend_from_slice(&nonce);
        payload.extend_from_slice(&ciphertext);
        Ok(base64::engine::general_purpose::STANDARD_NO_PAD.encode(payload))
    }

    pub fn decrypt_from_base64_no_pad(&self, encoded: &str) -> GlobalResult<String> {
        let payload = base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(encoded)
            .hand_log(|err| error!("{err}"))?;
        if payload.len() < 1 + 12 + 16 || payload[0] != 1 {
            return Err(exception::GlobalError::new_sys_error(
                "invalid AES-256-GCM payload",
                |err| error!("{err}"),
            ));
        }
        let mut nonce = Nonce::<U12>::default();
        nonce.copy_from_slice(&payload[1..13]);
        let plaintext = self.cipher.decrypt(&nonce, &payload[13..]).map_err(|_| {
            exception::GlobalError::new_sys_error("AES-256-GCM decrypt failed", |err| {
                error!("{err}")
            })
        })?;
        String::from_utf8(plaintext).hand_log(|err| error!("{err}"))
    }
}

pub fn generate_token(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..16]) // 32 字节（64 个十六进制字符）
}
fn gen_ascii_chars(size: usize) -> GlobalResult<String> {
    let mut rng = &mut rand::thread_rng();
    let string = String::from_utf8(
        BASE_STR
            .as_bytes()
            .choose_multiple(&mut rng, size)
            .cloned()
            .collect(),
    )
    .hand_log(|err| error!("{err}"))?;
    Ok(string)
}

fn encrypt(key: &str, data: &str) -> GlobalResult<String> {
    let iv_str = gen_ascii_chars(16)?;
    let iv = iv_str.as_bytes();
    let cipher = AesCbc::new_from_slices(key.as_bytes(), iv).hand_log(|err| error!("{err}"))?;
    let ciphertext = cipher.encrypt_vec(data.as_bytes());
    let mut buffer = bytebuffer::ByteBuffer::from_bytes(iv);
    buffer.write_bytes(&ciphertext);
    Ok(base64::engine::general_purpose::STANDARD.encode(buffer.to_bytes()))
}

fn decrypt(key: &str, data: &str) -> GlobalResult<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .hand_log(|err| error!("{err}"))?;
    let cipher =
        AesCbc::new_from_slices(key.as_bytes(), &bytes[0..16]).hand_log(|err| error!("{err}"))?;
    let string = String::from_utf8(
        cipher
            .decrypt_vec(&bytes[16..])
            .hand_log(|err| error!("{err}"))?,
    )
    .hand_log(|err| error!("{err}"))?;
    Ok(string)
}

pub fn default_encrypt(data: &str) -> GlobalResult<String> {
    encrypt(DEFAULT_KEY, data)
}

pub fn default_decrypt(data: &str) -> GlobalResult<String> {
    decrypt(DEFAULT_KEY, data)
}

#[cfg(test)]
mod test {
    use base64::Engine;

    use crate::utils::crypto::{
        decrypt, default_decrypt, default_encrypt, encrypt, generate_token, Aes256GcmCipher,
    };

    #[test]
    fn t1() {
        let plaintext = "hello world";
        let key = "01234567012345670123456701234567";
        let enc = encrypt(key, plaintext);
        println!("{:?}", enc);
        let dec = decrypt(key, &enc.unwrap());
        println!("{:?}", dec);
    }

    #[test]
    fn t2() {
        let plaintext = "Ms@2023%Kht";
        let enc = default_encrypt(plaintext).unwrap();
        let dec = default_decrypt(&enc).unwrap();
        println!("dec = {},enc = {}", dec, enc);
        println!(
            "{}",
            default_decrypt("Zncyb25BdWFZQkhxZ3JHST/4t3MN5NMWNZT3HVjNxRY=").unwrap()
        );
    }

    #[test]
    fn t3() {
        let text = "asdfa1231asdfassJKJKLJKL.";
        let string = generate_token(text);
        println!("{}", string);
    }
    #[test]
    fn t4() {
        println!(
            "{}",
            default_decrypt("clRXVjIzU1VrS3BEMXZmNxp5adMgQy599aQeu0tHYg0=").unwrap()
        );
    }

    #[test]
    fn aes_256_gcm_uses_random_nonce_and_detects_wrong_key() {
        let key = base64::engine::general_purpose::STANDARD_NO_PAD.encode([7_u8; 32]);
        let other_key = base64::engine::general_purpose::STANDARD_NO_PAD.encode([8_u8; 32]);
        let cipher = Aes256GcmCipher::from_base64_key_no_pad(&key).unwrap();
        let encrypted = cipher.encrypt_to_base64_no_pad("mqtt-password").unwrap();
        assert_ne!(
            encrypted,
            cipher.encrypt_to_base64_no_pad("mqtt-password").unwrap()
        );
        assert_eq!(
            cipher.decrypt_from_base64_no_pad(&encrypted).unwrap(),
            "mqtt-password"
        );
        assert!(Aes256GcmCipher::from_base64_key_no_pad(&other_key)
            .unwrap()
            .decrypt_from_base64_no_pad(&encrypted)
            .is_err());
    }
}

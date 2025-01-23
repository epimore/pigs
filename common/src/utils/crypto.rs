use aes::Aes256;
use block_modes::{BlockMode, Cbc};
use block_modes::block_padding::Pkcs7;
use log::error;
use rand::seq::SliceRandom;

use exception::{GlobalResult, TransError};

type AesCbc = Cbc<Aes256, Pkcs7>;

const BASE_STR: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
const DEFAULT_KEY: &str = "1234567890All in Rust 1234567890";//32长度

fn gen_ascii_chars(size: usize) -> GlobalResult<String> {
    let mut rng = &mut rand::thread_rng();
    let string = String::from_utf8(
        BASE_STR.as_bytes()
            .choose_multiple(&mut rng, size)
            .cloned()
            .collect()
    ).hand_log(|err|error!("{err}"))?;
    Ok(string)
}

fn encrypt(key: &str, data: &str) -> GlobalResult<String> {
    let iv_str = gen_ascii_chars(16)?;
    let iv = iv_str.as_bytes();
    let cipher = AesCbc::new_from_slices(key.as_bytes(), iv).hand_log(|err|error!("{err}"))?;
    let ciphertext = cipher.encrypt_vec(data.as_bytes());
    let mut buffer = bytebuffer::ByteBuffer::from_bytes(iv);
    buffer.write_bytes(&ciphertext);
    Ok(base64::encode(buffer.to_bytes()))
}

fn decrypt(key: &str, data: &str) -> GlobalResult<String> {
    let bytes = base64::decode(data).hand_log(|err|error!("{err}"))?;
    let cipher = AesCbc::new_from_slices(key.as_bytes(), &bytes[0..16]).hand_log(|err|error!("{err}"))?;
    let string = String::from_utf8(cipher.decrypt_vec(&bytes[16..]).hand_log(|err|error!("{err}"))?).hand_log(|err|error!("{err}"))?;
    Ok(string)
}

pub fn default_encrypt(data: &str) -> GlobalResult<String> {
    encrypt(DEFAULT_KEY, data)
}

pub fn default_decrypt(data: &str) -> GlobalResult<String> {
    decrypt(DEFAULT_KEY, data)
}

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
    println!("{}",default_decrypt("Zncyb25BdWFZQkhxZ3JHST/4t3MN5NMWNZT3HVjNxRY=").unwrap());
}
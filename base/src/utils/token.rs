use std::time::{SystemTime, UNIX_EPOCH};
use std::process;
use base64::Engine;

const NONCE: &str = "Gmv123...";

/// 生成token（base64编码返回）
pub fn encode_token(id: &str, sec: &str, noc: &str) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let pid = process::id();
    let raw = format!("{id}.{sec}.{now}.{pid}");
    let sig = simple_hash(&(raw.clone() + noc));
    let token_raw = format!("{id}.{sec}.{now}.{pid}.{sig:016x}").into_bytes();
    let obf = obfuscate(token_raw, 0x5A);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(obf)
}

pub fn default_token(id: &str, sec: &str) -> String {
    encode_token(id, sec, NONCE)
}
/// 验签
pub fn verify_token(token: &str, key: &str) -> bool {
    let obf = match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(token) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let raw_bytes = deobfuscate(obf, 0x5A);
    let raw = match String::from_utf8(raw_bytes) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let parts: Vec<&str> = raw.rsplitn(2, '.').collect();
    if parts.len() != 2 { return false; }
    let sig = parts[0];
    let raw_str = &raw[..raw.len() - sig.len() - 1];
    let expected = format!("{:016x}", simple_hash(&(raw_str.to_string() + key)));
    sig == expected
}

fn obfuscate(mut data: Vec<u8>, mask: u8) -> Vec<u8> {
    data.reverse();
    for b in &mut data {
        *b ^= mask;
    }
    data
}

fn deobfuscate(mut data: Vec<u8>, mask: u8) -> Vec<u8> {
    for b in &mut data {
        *b ^= mask;
    }
    data.reverse();
    data
}

/// 简单 hash（DJB2）
fn simple_hash(s: &str) -> u64 {
    let mut hash = 5381u64;
    for b in s.bytes() {
        hash = ((hash << 5).wrapping_add(hash)).wrapping_add(b as u64);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_randomness_and_verify() {
        let id = "alice";
        let sec = "gmv";
        let noc = NONCE;
        // let noc = "super_secret_key";

        let token1 = encode_token(id, sec, noc);
        let token2 = encode_token(id, sec, noc);

        println!("token1: {}", token1);
        println!("token2: {}", token2);

        // 每次生成应不同
        assert_ne!(token1, token2);

        // 验签应通过
        assert!(verify_token(&token1, noc));
        assert!(verify_token(&token2, noc));

        // 错误key应无法通过
        assert!(!verify_token(&token1, "wrong_key"));

        // 篡改token应无法通过
        let mut chars: Vec<char> = token1.chars().collect();
        if let Some(last) = chars.last_mut() {
            *last = if *last == 'a' { 'b' } else { 'a' };
        }
        let tampered = chars.into_iter().collect::<String>();
        assert!(!verify_token(&tampered, noc));
    }
}
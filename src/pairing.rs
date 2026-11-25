use anyhow::Result;
use rand::RngExt;
use rand::distr::{Alphanumeric, Distribution};
use sha2::{Digest, Sha256};

use crate::state::{Db, now};

const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

pub fn hash_code(code: &str) -> String {
    let normalized: String = code
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_uppercase())
        .collect();
    let digest = Sha256::digest(normalized.as_bytes());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

pub fn generate_code() -> String {
    let mut rng = rand::rng();
    let raw: String = (0..8)
        .map(|_| {
            let index = rng.random_range(0..ALPHABET.len());
            ALPHABET[index] as char
        })
        .collect();
    format!("{}-{}", &raw[..4], &raw[4..])
}

pub async fn create_pairing_code(db: &Db, instance_id: &str, ttl_secs: u64) -> Result<String> {
    let code = generate_code();
    let expires_at = now() + ttl_secs as i64;
    db.create_pairing_code(instance_id, &hash_code(&code), expires_at)
        .await?;
    Ok(code)
}

pub async fn consume_pairing_code(db: &Db, code: &str) -> Result<Option<String>> {
    db.consume_pairing_code(&hash_code(code)).await
}

#[allow(dead_code)]
fn random_string(length: usize) -> String {
    Alphanumeric
        .sample_iter(rand::rng())
        .take(length)
        .map(char::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_hash_is_normalized() {
        assert_eq!(hash_code("abcd-efgh"), hash_code("ABCD EFGH"));
    }

    #[test]
    fn generated_code_has_expected_shape() {
        let code = generate_code();
        assert_eq!(code.len(), 9);
        assert_eq!(code.chars().nth(4), Some('-'));
    }
}

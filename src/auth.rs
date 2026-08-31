use aes::cipher::KeyIvInit;
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, block_padding::Pkcs7};
use md5::{Digest as Md5Digest, Md5};
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use sha1::Sha1;
use sha2::Sha512;

pub type Aes192CbcEnc = cbc::Encryptor<aes::Aes192>;
pub type Aes192CbcDec = cbc::Decryptor<aes::Aes192>;
pub type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
pub type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

const ZERO_IV: [u8; 16] = [0u8; 16];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verifier {
    V11G,
    V12C,
}

pub struct AuthState {
    pub verifier: Verifier,
    pub password: Vec<u8>,
    pub vfr_data: [u8; 16],
    pub server_a: Vec<u8>,
    pub client_b: Option<Vec<u8>>,
    pub csk_salt: Option<[u8; 16]>,
    pub vgen_count: u32,
    pub sder_count: u32,
}

impl AuthState {
    pub fn new_11g(password: impl Into<Vec<u8>>) -> Self {
        let mut vfr = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut vfr);
        let mut server_a = vec![0u8; 48];
        rand::thread_rng().fill_bytes(&mut server_a);
        Self {
            verifier: Verifier::V11G,
            password: password.into(),
            vfr_data: vfr,
            server_a,
            client_b: None,
            csk_salt: None,
            vgen_count: 0,
            sder_count: 0,
        }
    }

    pub fn new_12c(password: impl Into<Vec<u8>>) -> Self {
        let mut vfr = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut vfr);
        let mut server_a = vec![0u8; 32];
        rand::thread_rng().fill_bytes(&mut server_a);
        let mut csk_salt = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut csk_salt);
        Self {
            verifier: Verifier::V12C,
            password: password.into(),
            vfr_data: vfr,
            server_a,
            client_b: None,
            csk_salt: Some(csk_salt),
            vgen_count: 4096,
            sder_count: 3,
        }
    }

    pub fn phase_one_sesskey(&self) -> Vec<u8> {
        match self.verifier {
            Verifier::V11G => {
                let key = password_hash_11g(&self.password, &self.vfr_data);
                aes192_encrypt_no_padding(&key, &self.server_a)
            }
            Verifier::V12C => {
                let key = password_hash_12c(&self.password, &self.vfr_data, self.vgen_count);
                aes256_encrypt_no_padding(&key, &self.server_a)
            }
        }
    }

    pub fn set_client_sesskey(&mut self, client_sesskey_hex: &str) -> Result<(), String> {
        let ciphertext = hex::decode(client_sesskey_hex).map_err(|e| e.to_string())?;
        let plaintext = match self.verifier {
            Verifier::V11G => {
                let key = password_hash_11g(&self.password, &self.vfr_data);
                // Client sends exactly 48 bytes (3 blocks) with no padding.
                aes192_decrypt_no_padding(&key, &ciphertext)?
            }
            Verifier::V12C => {
                let key = password_hash_12c(&self.password, &self.vfr_data, self.vgen_count);
                // Client sends exactly 32 bytes (2 blocks) with no padding.
                aes256_decrypt_no_padding(&key, &ciphertext)?
            }
        };
        tracing::debug!("received and decrypted client session key");
        self.client_b = Some(plaintext);
        Ok(())
    }

    pub fn combo_key(&self) -> Vec<u8> {
        match self.verifier {
            Verifier::V11G => {
                let server_a = &self.server_a;
                let client_b = self.client_b.as_ref().expect("client_b not set");
                let xored: Vec<u8> = server_a[16..40]
                    .iter()
                    .zip(&client_b[16..40])
                    .map(|(a, b)| a ^ b)
                    .collect();
                let mut combo = md5(&xored[..16]);
                combo.extend_from_slice(&md5(&xored[16..])[..8]);
                combo
            }
            Verifier::V12C => {
                let server_a = &self.server_a;
                let client_b = self.client_b.as_ref().expect("client_b not set");
                let combo_input = format!(
                    "{}{}",
                    hex::encode_upper(&client_b[..32]),
                    hex::encode_upper(&server_a[..32])
                );
                let salt = self.csk_salt.expect("csk_salt not set");
                pbkdf2_sha512(combo_input.as_bytes(), &salt, self.sder_count, 32)
            }
        }
    }

    pub fn verify_password(&self, auth_password_hex: &str) -> Result<(), String> {
        let ciphertext = hex::decode(auth_password_hex).map_err(|e| e.to_string())?;
        let combo_key = self.combo_key();
        tracing::debug!(
            ciphertext_len = ciphertext.len(),
            "verifying password proof"
        );
        let plaintext = match self.verifier {
            Verifier::V11G => aes192_decrypt_pkcs7(&combo_key, &ciphertext)?,
            Verifier::V12C => aes256_decrypt_pkcs7(&combo_key, &ciphertext)?,
        };
        tracing::debug!(plaintext_len = plaintext.len(), "password proof decrypted");
        if plaintext.len() < 16 + self.password.len() {
            return Err("password plaintext too short".to_string());
        }
        if plaintext[16..16 + self.password.len()] != self.password[..] {
            return Err("password mismatch".to_string());
        }
        Ok(())
    }

    pub fn verify_speedy_key(&self, _speedy_key_hex: &str) -> Result<(), String> {
        if self.verifier == Verifier::V12C {
            let ciphertext = hex::decode(_speedy_key_hex).map_err(|e| e.to_string())?;
            let combo_key = self.combo_key();
            // oracle-rs sends only the first 80 bytes of the encrypted speedy
            // key. That deliberately omits the final PKCS#7 padding block, so
            // this field must be decrypted as complete AES blocks, not as a
            // standalone padded ciphertext.
            let plaintext = aes256_decrypt_no_padding(&combo_key, &ciphertext)?;
            let password_key = password_key_12c(&self.password, &self.vfr_data, self.vgen_count);
            if plaintext.len() < 16 + password_key.len() {
                return Err("speedy key plaintext too short".to_string());
            }
            if plaintext[16..16 + password_key.len()] != password_key[..] {
                return Err("speedy key mismatch".to_string());
            }
        }
        Ok(())
    }

    pub fn svr_response(&self) -> Vec<u8> {
        let combo_key = self.combo_key();
        // Plaintext is `[16 bytes any][b"SERVER_TO_CLIENT"]` (32 bytes). Real
        // Oracle then PKCS#7-pads it — a full extra block of `0x10` — before
        // encrypting, so the response is 48 bytes. python-oracledb / oracle-rs
        // decrypt raw and only inspect bytes [16..32], so they tolerate an
        // unpadded 32-byte response; ojdbc thin strips PKCS#5 padding and fails
        // ORA-17452 unless the padding block is present. Pad to match Oracle.
        let mut marker = vec![0u8; 16];
        marker.extend_from_slice(b"SERVER_TO_CLIENT");
        marker.extend_from_slice(&[0x10u8; 16]);
        match self.verifier {
            Verifier::V11G => aes192_encrypt_no_padding(&combo_key, &marker),
            Verifier::V12C => aes256_encrypt_no_padding(&combo_key, &marker),
        }
    }
}

fn password_hash_11g(password: &[u8], vfr: &[u8]) -> Vec<u8> {
    let mut hasher = Sha1::new();
    hasher.update(password);
    hasher.update(vfr);
    let mut hash = hasher.finalize().to_vec();
    hash.extend_from_slice(&[0u8; 4]);
    hash
}

fn password_key_12c(password: &[u8], vfr: &[u8], vgen: u32) -> Vec<u8> {
    let salt: Vec<u8> = [vfr, b"AUTH_PBKDF2_SPEEDY_KEY"].concat();
    pbkdf2_sha512(password, &salt, vgen, 64)
}

fn password_hash_12c(password: &[u8], vfr: &[u8], vgen: u32) -> Vec<u8> {
    let key = password_key_12c(password, vfr, vgen);
    let mut hasher = Sha512::new();
    hasher.update(&key);
    hasher.update(vfr);
    hasher.finalize()[..32].to_vec()
}

fn pbkdf2_sha512(password: &[u8], salt: &[u8], rounds: u32, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    pbkdf2_hmac::<Sha512>(password, salt, rounds, &mut out);
    out
}

fn md5(data: &[u8]) -> Vec<u8> {
    let mut hasher = Md5::new();
    hasher.update(data);
    hasher.finalize().to_vec()
}

fn aes192_encrypt_no_padding(key: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let cipher = Aes192CbcEnc::new(key.into(), &ZERO_IV.into());
    let mut buffer = plaintext.to_vec();
    let len = buffer.len();
    cipher
        .encrypt_padded_mut::<cbc::cipher::block_padding::NoPadding>(&mut buffer, len)
        .expect("encrypt")
        .to_vec()
}

fn aes192_decrypt_no_padding(key: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = Aes192CbcDec::new(key.into(), &ZERO_IV.into());
    let mut buffer = ciphertext.to_vec();
    cipher
        .decrypt_padded_mut::<cbc::cipher::block_padding::NoPadding>(&mut buffer)
        .map(|value| value.to_vec())
        .map_err(|_| "invalid AES-192 session-key ciphertext".to_string())
}

fn aes192_decrypt_pkcs7(key: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = Aes192CbcDec::new(key.into(), &ZERO_IV.into());
    let mut buffer = ciphertext.to_vec();
    cipher
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .map(|value| value.to_vec())
        .map_err(|_| "invalid AES-192 password proof".to_string())
}

fn aes256_encrypt_no_padding(key: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let cipher = Aes256CbcEnc::new(key.into(), &ZERO_IV.into());
    let mut buffer = plaintext.to_vec();
    let len = buffer.len();
    cipher
        .encrypt_padded_mut::<cbc::cipher::block_padding::NoPadding>(&mut buffer, len)
        .expect("encrypt")
        .to_vec()
}

fn aes256_decrypt_no_padding(key: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = Aes256CbcDec::new(key.into(), &ZERO_IV.into());
    let mut buffer = ciphertext.to_vec();
    cipher
        .decrypt_padded_mut::<cbc::cipher::block_padding::NoPadding>(&mut buffer)
        .map(|value| value.to_vec())
        .map_err(|_| "invalid AES-256 session-key ciphertext".to_string())
}

fn aes256_decrypt_pkcs7(key: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = Aes256CbcDec::new(key.into(), &ZERO_IV.into());
    let mut buffer = ciphertext.to_vec();
    cipher
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .map(|value| value.to_vec())
        .map_err(|_| "invalid AES-256 password proof".to_string())
}

pub fn hex_upper(data: &[u8]) -> String {
    hex::encode_upper(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_11g_known_vector() {
        let password = b"pass";
        let vfr = hex::decode("0123456789ABCDEF0123456789ABCDEF").unwrap();
        let server_a: Vec<u8> = (0..48u8).collect();

        let state = AuthState {
            verifier: Verifier::V11G,
            password: password.to_vec(),
            vfr_data: vfr.clone().try_into().unwrap(),
            server_a: server_a.clone(),
            client_b: None,
            csk_salt: None,
            vgen_count: 0,
            sder_count: 0,
        };

        let password_hash = password_hash_11g(password, &vfr);
        assert_eq!(
            hex_upper(&password_hash),
            "8B6258B34E2172A80A651448A6CB981F19A4938E00000000"
        );

        let sesskey = state.phase_one_sesskey();
        assert_eq!(
            hex_upper(&sesskey),
            "9E61544C6A37C0AB46A5D026518E3C2592D981DC8BC09C61A1D3336ADF3A8E03B90FDD5DA77648D6BC40D75D3D838D84"
        );
    }
}

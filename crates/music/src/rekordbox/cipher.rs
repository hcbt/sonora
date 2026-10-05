//! SQLCipher for a rekordbox `master.db`.
//!
//! Rekordbox 6 and 7 store the collection in a SQLCipher database. The passphrase is the same
//! for every installation: the program has to open the file on the machine that holds it, and
//! the key it uses is not tied to a license. A file that is already a plain SQLite database is
//! returned as it is, so a decrypted copy and an export both open.
//!
//! Writing a playlist back uses the same passphrase. [`open`] keeps the derived key so a later
//! [`Seal::lock`] does not run the key derivation again.

use aes::Aes256;
use aes::cipher::{Array, BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use anyhow::{Result, bail};
use hmac::Hmac;
use hmac::digest::block_api::EagerHash;
use hmac::digest::{FixedOutputReset, Mac};
use pbkdf2::pbkdf2_hmac;
use pbkdf2::sha2::Sha512;
use sha1::Sha1;

/// The passphrase rekordbox passes to `sqlite3_key` for `master.db`.
const PASSPHRASE: &[u8] = b"402fd482c38817c35ffa8ffb8c7d93143b749e7d315df7a81732a1ff43608497";

const SQLITE_HEADER: &[u8] = b"SQLite format 3\0";
const SALT_LEN: usize = 16;
const AES_BLOCK: usize = 16;
/// SQLCipher derives the HMAC key with the encryption salt XOR'd by this byte.
const HMAC_SALT_MASK: u8 = 0x3a;
const HMAC_KDF_ROUNDS: u32 = 2;

/// SQLCipher 4, which rekordbox 6 and 7 write.
const V4: Params = Params {
    page_size: 4096,
    kdf_rounds: 256_000,
    sha512: true,
};

/// SQLCipher 3, tried when a file was written before the v4 defaults.
const V3: Params = Params {
    page_size: 1024,
    kdf_rounds: 64_000,
    sha512: false,
};

#[derive(Clone, Copy)]
struct Params {
    page_size: usize,
    kdf_rounds: u32,
    /// SHA-512 for SQLCipher 4, SHA-1 for SQLCipher 3. The HMAC and the key derivation use the
    /// same hash.
    sha512: bool,
}

impl Params {
    fn hmac_size(self) -> usize {
        match self.sha512 {
            true => 64,
            false => 20,
        }
    }

    fn reserve(self) -> usize {
        let raw = AES_BLOCK + self.hmac_size();
        raw.div_ceil(AES_BLOCK) * AES_BLOCK
    }
}

/// The key material of one database, kept so a playlist edit can lock the file again without
/// deriving the key a second time.
#[derive(Clone)]
pub struct Seal {
    salt: [u8; SALT_LEN],
    key: [u8; 32],
    hmac_key: [u8; 32],
    params: Params,
}

impl Seal {
    /// Locks `plain` with this database's key. `plain` must be the SQLite image [`open`]
    /// returned, possibly after a playlist edit, and a whole number of pages.
    pub fn lock(&self, plain: &[u8]) -> Result<Vec<u8>> {
        if !plain.starts_with(SQLITE_HEADER) {
            bail!("the rekordbox database is not a SQLite file");
        }
        if !plain.len().is_multiple_of(self.params.page_size) {
            bail!("the rekordbox database is not a whole number of pages");
        }
        let reserve = self.params.reserve();
        let pages = plain.len() / self.params.page_size;
        let mut locked = Vec::with_capacity(plain.len());
        for number in 1..=pages {
            let start = (number - 1) * self.params.page_size;
            let page = &plain[start..start + self.params.page_size];
            locked.extend(encrypt_page(
                page,
                number as u32,
                &self.salt,
                &self.key,
                &self.hmac_key,
                self.params,
                reserve,
            )?);
        }
        Ok(locked)
    }

    /// Bytes at the end of each page that SQLCipher keeps for the IV and the HMAC. SQLite must
    /// leave them unused, or locking the file would drop them.
    pub fn reserve(&self) -> usize {
        self.params.reserve()
    }

    pub fn page_size(&self) -> usize {
        self.params.page_size
    }

    /// Decrypts `bytes` with this key. `None` when the file was rewritten with a different salt,
    /// so the caller can open it from scratch.
    pub fn reveal(&self, bytes: &[u8]) -> Option<Vec<u8>> {
        if bytes.starts_with(SQLITE_HEADER) {
            return Some(bytes.to_vec());
        }
        if bytes.len() < SALT_LEN || bytes[..SALT_LEN] != self.salt {
            return None;
        }
        if !bytes.len().is_multiple_of(self.params.page_size) {
            return None;
        }
        let pages = bytes.len() / self.params.page_size;
        let mut plain = Vec::with_capacity(bytes.len());
        for number in 1..=pages {
            let start = (number - 1) * self.params.page_size;
            let page = &bytes[start..start + self.params.page_size];
            plain.extend(decrypt_page(
                page,
                number as u32,
                &self.key,
                &self.hmac_key,
                self.params,
            )?);
        }
        plain.starts_with(SQLITE_HEADER).then_some(plain)
    }
}

/// Opens `bytes` and keeps the key that locked it. A plain SQLite file gets a fresh SQLCipher 4
/// key, so the first playlist edit can still be written in the form rekordbox opens.
pub fn open(bytes: &[u8]) -> Result<(Vec<u8>, Seal)> {
    if bytes.starts_with(SQLITE_HEADER) {
        let page_size = page_size(bytes)?;
        return Ok((bytes.to_vec(), Seal::fresh(page_size)));
    }
    if bytes.len() < SALT_LEN {
        bail!("the rekordbox database is too small to be a database");
    }

    for params in [V4, V3] {
        if !bytes.len().is_multiple_of(params.page_size) {
            continue;
        }
        if let Some((plain, seal)) = decrypt(bytes, params) {
            return Ok((plain, seal));
        }
    }
    bail!("cannot unlock the rekordbox database")
}

fn page_size(bytes: &[u8]) -> Result<usize> {
    let raw = bytes
        .get(16..18)
        .and_then(|field| field.try_into().ok())
        .map(u16::from_be_bytes)
        .unwrap_or(0);
    let size = match raw {
        1 => 65_536,
        0 => 0,
        value => value as usize,
    };
    if size < 512 || !size.is_multiple_of(AES_BLOCK) {
        bail!("the rekordbox database has no page size");
    }
    Ok(size)
}

impl Seal {
    fn fresh(page_size: usize) -> Self {
        let mut salt = [0u8; SALT_LEN];
        fastrand::fill(&mut salt);
        let params = Params { page_size, ..V4 };
        Self::from_salt(salt, params)
    }

    fn from_salt(salt: [u8; SALT_LEN], params: Params) -> Self {
        let key = derive(params, PASSPHRASE, &salt, params.kdf_rounds);
        let mut hmac_salt = [0u8; SALT_LEN];
        for (out, byte) in hmac_salt.iter_mut().zip(salt) {
            *out = byte ^ HMAC_SALT_MASK;
        }
        let hmac_key = derive(params, &key, &hmac_salt, HMAC_KDF_ROUNDS);
        Self {
            salt,
            key,
            hmac_key,
            params,
        }
    }
}

/// Decrypts every page. `None` when the first page does not come out as a SQLite header, which
/// is how a wrong passphrase or the wrong cipher version is told apart from a real database.
fn decrypt(bytes: &[u8], params: Params) -> Option<(Vec<u8>, Seal)> {
    let salt: [u8; SALT_LEN] = bytes.get(..SALT_LEN)?.try_into().ok()?;
    let seal = Seal::from_salt(salt, params);

    let pages = bytes.len() / params.page_size;
    let mut plain = Vec::with_capacity(bytes.len());
    for number in 1..=pages {
        let start = (number - 1) * params.page_size;
        let page = &bytes[start..start + params.page_size];
        plain.extend(decrypt_page(
            page,
            number as u32,
            &seal.key,
            &seal.hmac_key,
            params,
        )?);
    }
    plain.starts_with(SQLITE_HEADER).then_some((plain, seal))
}

fn derive(params: Params, password: &[u8], salt: &[u8], rounds: u32) -> [u8; 32] {
    let mut key = [0u8; 32];
    match params.sha512 {
        true => pbkdf2_hmac::<Sha512>(password, salt, rounds, &mut key),
        false => pbkdf2_hmac::<Sha1>(password, salt, rounds, &mut key),
    }
    key
}

fn decrypt_page(
    page: &[u8],
    number: u32,
    key: &[u8; 32],
    hmac_key: &[u8; 32],
    params: Params,
) -> Option<Vec<u8>> {
    let reserve = params.reserve();
    let iv_at = params.page_size - reserve;
    let body_at = if number == 1 { SALT_LEN } else { 0 };
    if iv_at < body_at + AES_BLOCK {
        return None;
    }

    let iv = page.get(iv_at..iv_at + AES_BLOCK)?;
    let stored = page.get(iv_at + AES_BLOCK..iv_at + AES_BLOCK + params.hmac_size())?;
    let signed = page.get(body_at..iv_at + AES_BLOCK)?;
    let matches = match params.sha512 {
        true => mac_ok::<Sha512>(hmac_key, signed, number, stored),
        false => mac_ok::<Sha1>(hmac_key, signed, number, stored),
    };
    if !matches {
        return None;
    }

    let encrypted = page.get(body_at..iv_at)?;
    let decoded = cbc_decrypt(key, iv, encrypted).ok()?;
    let mut plain = Vec::with_capacity(params.page_size);
    // The salt stands in for the first 16 bytes of the SQLite header. Putting the header back
    // is what makes page 1 a database a normal SQLite reader can open.
    if number == 1 {
        plain.extend_from_slice(SQLITE_HEADER);
    }
    plain.extend_from_slice(&decoded);
    plain.resize(params.page_size, 0);
    Some(plain)
}

fn encrypt_page(
    page: &[u8],
    number: u32,
    salt: &[u8; SALT_LEN],
    key: &[u8; 32],
    hmac_key: &[u8; 32],
    params: Params,
    reserve: usize,
) -> Result<Vec<u8>> {
    let iv_at = params.page_size - reserve;
    let body_at = if number == 1 { SALT_LEN } else { 0 };
    if iv_at < body_at + AES_BLOCK || page.len() != params.page_size {
        bail!("a rekordbox page is the wrong size");
    }
    let mut iv = [0u8; AES_BLOCK];
    fastrand::fill(&mut iv);
    let encrypted = cbc_encrypt(key, &iv, &page[body_at..iv_at])?;
    let signed = [encrypted.as_slice(), iv.as_slice()].concat();
    let mac = match params.sha512 {
        true => mac_bytes::<Sha512>(hmac_key, &signed, number)?,
        false => mac_bytes::<Sha1>(hmac_key, &signed, number)?,
    };

    let mut locked = vec![0u8; params.page_size];
    if number == 1 {
        locked[..SALT_LEN].copy_from_slice(salt);
    }
    locked[body_at..iv_at].copy_from_slice(&encrypted);
    locked[iv_at..iv_at + AES_BLOCK].copy_from_slice(&iv);
    locked[iv_at + AES_BLOCK..iv_at + AES_BLOCK + mac.len()].copy_from_slice(&mac);
    Ok(locked)
}

fn mac_ok<D>(key: &[u8], message: &[u8], page: u32, stored: &[u8]) -> bool
where
    D: EagerHash + FixedOutputReset,
    Hmac<D>: Mac,
{
    let Ok(mut mac) = Hmac::<D>::new_from_slice(key) else {
        return false;
    };
    mac.update(message);
    mac.update(&page.to_le_bytes());
    mac.verify_slice(stored).is_ok()
}

fn mac_bytes<D>(key: &[u8], message: &[u8], page: u32) -> Result<Vec<u8>>
where
    D: EagerHash + FixedOutputReset,
    Hmac<D>: Mac,
{
    let mut mac = Hmac::<D>::new_from_slice(key)
        .map_err(|_| anyhow::anyhow!("cannot sign a rekordbox page"))?;
    mac.update(message);
    mac.update(&page.to_le_bytes());
    Ok(mac.finalize().into_bytes().to_vec())
}

fn cbc_decrypt(key: &[u8; 32], iv: &[u8], encrypted: &[u8]) -> Result<Vec<u8>> {
    if !encrypted.len().is_multiple_of(AES_BLOCK) {
        bail!("a rekordbox page is not a whole number of cipher blocks");
    }
    let key = Array::from(*key);
    let cipher = Aes256::new(&key);
    let mut previous = [0u8; AES_BLOCK];
    previous.copy_from_slice(iv);
    let mut plain = Vec::with_capacity(encrypted.len());
    for chunk in encrypted.chunks(AES_BLOCK) {
        let mut block = Array::from_fn(|index| chunk[index]);
        cipher.decrypt_block(&mut block);
        for (byte, prev) in block.iter_mut().zip(previous) {
            *byte ^= prev;
        }
        previous.copy_from_slice(chunk);
        plain.extend_from_slice(block.as_slice());
    }
    Ok(plain)
}

fn cbc_encrypt(key: &[u8; 32], iv: &[u8], plain: &[u8]) -> Result<Vec<u8>> {
    if !plain.len().is_multiple_of(AES_BLOCK) {
        bail!("a rekordbox page is not a whole number of cipher blocks");
    }
    let key = Array::from(*key);
    let cipher = Aes256::new(&key);
    let mut previous = [0u8; AES_BLOCK];
    previous.copy_from_slice(iv);
    let mut encrypted = Vec::with_capacity(plain.len());
    for chunk in plain.chunks(AES_BLOCK) {
        let mut block = Array::from_fn(|index| chunk[index] ^ previous[index]);
        cipher.encrypt_block(&mut block);
        previous.copy_from_slice(block.as_slice());
        encrypted.extend_from_slice(block.as_slice());
    }
    Ok(encrypted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_roundtrips_the_fixture() {
        let (plain, seal) = open(include_bytes!("fixture.db")).expect("open");
        let locked = seal.lock(&plain).expect("lock");
        assert!(!locked.starts_with(SQLITE_HEADER));
        let (again, _) = open(&locked).expect("unlock locked");
        assert_eq!(again, plain);
        assert_eq!(plain[20], 80, "reserved byte");
    }
}

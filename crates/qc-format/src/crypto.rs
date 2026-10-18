//! NDSP file encryption.
//!
//! Every encrypted NDSP file (blobs, `ModelRepo.xml`, `moduleReport.xml`,
//! kernel LDRs) uses the same scheme with a different secret:
//!
//! ```text
//! d1 = SHA1^10(secret)          d2 = SHA1^10(d1 ‖ secret)
//! AES-128-CTR, key = d1[0..16], initial counter = (d1 ‖ d2)[16..32] (128-bit big-endian)
//! ```
//!
//! The secrets (repo key, blob key, report seed + serial) come from the
//! user's own device; this module never stores or derives them.

use aes::cipher::{KeyIvInit, StreamCipher};
use sha1::{Digest, Sha1};
use zeroize::Zeroize;

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

/// AES key and initial counter derived from one NDSP secret. Both are zeroed on drop, so
/// share one value (for example behind an `Arc`) rather than cloning it.
#[derive(Clone)]
pub struct NdspCipher {
    key: [u8; 16],
    counter: [u8; 16],
}

impl NdspCipher {
    pub fn derive(secret: &[u8]) -> Self {
        let sha1_10 = |prefix: &[u8]| {
            let mut digest = Sha1::new()
                .chain_update(prefix)
                .chain_update(secret)
                .finalize();
            for _ in 1..10 {
                digest = Sha1::digest(digest);
            }
            digest
        };
        let mut d1 = sha1_10(&[]);
        let mut d2 = sha1_10(&d1);
        let mut cipher = Self {
            key: [0; 16],
            counter: [0; 16],
        };
        cipher.key.copy_from_slice(&d1[..16]);
        cipher.counter[..4].copy_from_slice(&d1[16..20]);
        cipher.counter[4..].copy_from_slice(&d2[..12]);
        d1.as_mut_slice().zeroize();
        d2.as_mut_slice().zeroize();
        cipher
    }

    /// Encrypts or decrypts in place (CTR mode is symmetric).
    pub fn apply_keystream(&self, data: &mut [u8]) {
        Aes128Ctr::new(&self.key.into(), &self.counter.into()).apply_keystream(data);
    }

    /// Encrypts or decrypts into a new buffer.
    pub fn crypt(&self, data: &[u8]) -> Vec<u8> {
        let mut out = data.to_vec();
        self.apply_keystream(&mut out);
        out
    }
}

impl Drop for NdspCipher {
    fn drop(&mut self) {
        self.key.zeroize();
        self.counter.zeroize();
    }
}

impl std::fmt::Debug for NdspCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NdspCipher(<redacted>)")
    }
}

/// Encrypts or decrypts `data` with the key derived from `secret`.
pub fn crypt(data: &[u8], secret: &[u8]) -> Vec<u8> {
    NdspCipher::derive(secret).crypt(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Vectors from Python's `cryptography` package running the toolkit's
    /// derivation with synthetic secrets (no device keys).
    #[test]
    fn matches_python_oracle() {
        let cipher = NdspCipher::derive(b"test-secret-123");
        assert_eq!(hex(&cipher.key), "28b24384669f3a1edb9a682cad4699e5");
        assert_eq!(hex(&cipher.counter), "cd5f988f19b24c3cde48906de13059c0");
        assert_eq!(
            hex(&cipher.crypt(b"Quad Cortex known-answer test!!!")),
            "06376ded0e818033a8e0b27a2f9aefbe9144ce4e8ab2a76706aa6efe695bb634"
        );

        // 77 bytes: several counter blocks plus a partial final block.
        let secret = b"qcdsp synthetic secret, not a device key";
        let plain: Vec<u8> = (0..77).collect();
        let encrypted = crypt(&plain, secret);
        assert_eq!(
            hex(&encrypted),
            "09fe944cd221d3642fc6940b08e0aab13e756a8c701e650a5df09125ca09e1e4\
             678634dab35548a73836611ff197ece6f1a915b00e9ed13e29e01fff4d7e8782\
             e4533c24cbf28cbd9fada9de65"
        );
        assert_eq!(crypt(&encrypted, secret), plain);

        let cipher = NdspCipher::derive(&[0; 16]);
        assert_eq!(hex(&cipher.key), "d5b59bfd8bc1058c9d9b98736e42f0cf");
        assert_eq!(hex(&cipher.counter), "e467bb1fa638c9c80a083dfb0ec3dce6");
        assert_eq!(
            hex(&cipher.crypt(b"<?xml version=\"1.0\"?>")),
            "f46193cdcf0b32aff1f9dff73f10ba2b35adbd16eb"
        );
    }

    /// The key and counter must be zeroed on drop: keep the `Drop` impl.
    #[test]
    fn cipher_has_drop_glue() {
        assert!(std::mem::needs_drop::<NdspCipher>());
    }
}

//! Web Push message encryption (RFC 8291): the `aes128gcm` content coding (RFC 8188) in one
//! record, for one device's keys. Only the device can read what it carries; the push service in
//! between can't.

use aws_lc_rs::{aead, agreement, hkdf};

/// The record size the header says (a push message is one record).
const RECORD_SIZE: u32 = 4096;
/// A P-256 public key, uncompressed (`0x04 || x || y`).
const POINT: usize = 65;
/// The header: salt (16), record size (4), key id length (1), the server's public key (65).
const HEADER: usize = 16 + 4 + 1 + POINT;
/// The most a push message carries: what fits in 4096 bytes with the header, the padding
/// delimiter and the AES-GCM tag.
pub const MAX_PLAINTEXT: usize = 4096 - HEADER - 1 - 16;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("the device's public key isn't a P-256 point")]
    BadKey,
    #[error("the device's auth secret isn't 16 bytes")]
    BadAuth,
    #[error("the message is {0} bytes; a push carries at most {MAX_PLAINTEXT}")]
    TooBig(usize),
    #[error("not a message for these keys")]
    NotOurs,
    #[error("the encryption library refused")]
    Crypto,
}

/// An output length for HKDF-Expand.
struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

/// HKDF-SHA-256: extract with `salt`, expand `info` (its parts joined) into `out`.
fn hkdf(salt: &[u8], ikm: &[u8], info: &[&[u8]], out: &mut [u8]) -> Result<(), Error> {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(ikm);
    prk.expand(info, Len(out.len())).map_err(|_| Error::Crypto)?.fill(out).map_err(|_| Error::Crypto)
}

/// The content key and nonce for one message (RFC 8291 §3.4): from the ECDH secret, the device's
/// auth secret and both public keys, then the message's salt.
fn derive(ecdh_secret: &[u8], auth: &[u8], ua_public: &[u8], as_public: &[u8], salt: &[u8]) -> Result<([u8; 16], [u8; 12]), Error> {
    let mut ikm = [0u8; 32];
    hkdf(auth, ecdh_secret, &[b"WebPush: info\0", ua_public, as_public], &mut ikm)?;
    let mut cek = [0u8; 16];
    hkdf(salt, &ikm, &[b"Content-Encoding: aes128gcm\0"], &mut cek)?;
    let mut nonce = [0u8; 12];
    hkdf(salt, &ikm, &[b"Content-Encoding: nonce\0"], &mut nonce)?;
    Ok((cek, nonce))
}

fn check_device(ua_public: &[u8], auth: &[u8]) -> Result<(), Error> {
    if ua_public.len() != POINT || ua_public.first() != Some(&4) {
        return Err(Error::BadKey);
    }
    if auth.len() != 16 {
        return Err(Error::BadAuth);
    }
    Ok(())
}

/// `plaintext` encrypted for a device (its subscription's `p256dh` public key and `auth`
/// secret): the body of a push message sent with `Content-Encoding: aes128gcm`. Each message has
/// its own key pair and salt.
pub fn encrypt(plaintext: &[u8], ua_public: &[u8], auth: &[u8]) -> Result<Vec<u8>, Error> {
    let as_private = agreement::PrivateKey::generate(&agreement::ECDH_P256).map_err(|_| Error::Crypto)?;
    let mut salt = [0u8; 16];
    aws_lc_rs::rand::fill(&mut salt).map_err(|_| Error::Crypto)?;
    encrypt_with(plaintext, ua_public, auth, &as_private, &salt)
}

/// [`encrypt`] with the server's key pair and the salt given (RFC 8291's worked example uses
/// fixed ones).
fn encrypt_with(plaintext: &[u8], ua_public: &[u8], auth: &[u8], as_private: &agreement::PrivateKey, salt: &[u8; 16]) -> Result<Vec<u8>, Error> {
    if plaintext.len() > MAX_PLAINTEXT {
        return Err(Error::TooBig(plaintext.len()));
    }
    check_device(ua_public, auth)?;
    let as_public = as_private.compute_public_key().map_err(|_| Error::Crypto)?;
    let as_public = as_public.as_ref();
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, ua_public);
    let (cek, nonce) = agreement::agree(as_private, peer, Error::BadKey, |secret| derive(secret, auth, ua_public, as_public, salt))?;
    let key_id_len = u8::try_from(as_public.len()).map_err(|_| Error::Crypto)?;
    let mut out = Vec::with_capacity(HEADER + plaintext.len() + 17);
    out.extend_from_slice(salt);
    out.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    out.push(key_id_len);
    out.extend_from_slice(as_public);
    // One record, and the last: the plaintext, then its delimiter (0x02), no padding.
    let mut record = Vec::with_capacity(plaintext.len() + 17);
    record.extend_from_slice(plaintext);
    record.push(2);
    let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).map_err(|_| Error::Crypto)?);
    key.seal_in_place_append_tag(aead::Nonce::assume_unique_for_key(nonce), aead::Aad::empty(), &mut record).map_err(|_| Error::Crypto)?;
    out.extend_from_slice(&record);
    Ok(out)
}

/// What a device reads from a push message's body with its private key (raw, 32 bytes), its
/// public key and auth secret: kiln's own tests use it, as a fake push service's device.
pub fn decrypt(body: &[u8], ua_private: &[u8], ua_public: &[u8], auth: &[u8]) -> Result<Vec<u8>, Error> {
    check_device(ua_public, auth)?;
    let salt = body.get(..16).ok_or(Error::NotOurs)?;
    let id_len = usize::from(*body.get(20).ok_or(Error::NotOurs)?);
    let as_public = body.get(21..21 + id_len).ok_or(Error::NotOurs)?;
    let mut record = body.get(21 + id_len..).ok_or(Error::NotOurs)?.to_vec();
    let private = agreement::PrivateKey::from_private_key(&agreement::ECDH_P256, ua_private).map_err(|_| Error::BadKey)?;
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, as_public);
    let (cek, nonce) = agreement::agree(&private, peer, Error::NotOurs, |secret| derive(secret, auth, ua_public, as_public, salt))?;
    let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).map_err(|_| Error::Crypto)?);
    let plain = key.open_in_place(aead::Nonce::assume_unique_for_key(nonce), aead::Aad::empty(), &mut record).map_err(|_| Error::NotOurs)?;
    // The last record's delimiter is 0x02, after the content and any zero padding.
    let end = plain.iter().rposition(|b| *b != 0).ok_or(Error::NotOurs)?;
    if plain.get(end) != Some(&2) {
        return Err(Error::NotOurs);
    }
    Ok(plain.get(..end).ok_or(Error::NotOurs)?.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn b64(s: &str) -> Vec<u8> {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s.split_whitespace().collect::<String>()).unwrap_or_default()
    }

    /// RFC 8291, Section 5 and Appendix A: the worked example, byte for byte.
    #[test]
    fn rfc8291_appendix_a() -> anyhow::Result<()> {
        let plaintext = b64("V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24");
        assert_eq!(plaintext, b"When I grow up, I want to be a watermelon");
        let as_private = agreement::PrivateKey::from_private_key(&agreement::ECDH_P256, &b64("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw")).map_err(|e| anyhow::anyhow!("{e}"))?;
        let as_public = as_private.compute_public_key().map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(as_public.as_ref(), b64("BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIg Dll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8").as_slice());
        let ua_public = b64("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcx aOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4");
        let ua_private = b64("q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94");
        let auth = b64("BTBZMqHH6r4Tts7J_aSIgg");
        let salt: [u8; 16] = b64("DGv6ra1nlYgDCS1FRnbzlw").try_into().map_err(|_| anyhow::anyhow!("salt"))?;

        // The intermediate values.
        let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, ua_public.as_slice());
        let secret = agreement::agree(&as_private, peer, Error::BadKey, |s| Ok(s.to_vec()))?;
        assert_eq!(secret, b64("kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs"));
        let mut ikm = [0u8; 32];
        hkdf(&auth, &secret, &[b"WebPush: info\0", &ua_public, as_public.as_ref()], &mut ikm)?;
        assert_eq!(ikm.to_vec(), b64("S4lYMb_L0FxCeq0WhDx813KgSYqU26kOyzWUdsXYyrg"));
        let (cek, nonce) = derive(&secret, &auth, &ua_public, as_public.as_ref(), &salt)?;
        assert_eq!(cek.to_vec(), b64("oIhVW04MRdy2XN9CiKLxTg"));
        assert_eq!(nonce.to_vec(), b64("4h_95klXJ5E_qnoN"));

        // The message, as Section 5 shows it.
        let body = encrypt_with(&plaintext, &ua_public, &auth, &as_private, &salt)?;
        let want = b64(
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml
             mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPT
             pK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN",
        );
        assert_eq!(body.len(), 144);
        assert_eq!(body, want);
        // And the device reads it back.
        assert_eq!(decrypt(&body, &ua_private, &ua_public, &auth)?, plaintext);
        Ok(())
    }

    #[test]
    fn each_message_has_its_own_keys_and_only_the_device_reads_it() -> anyhow::Result<()> {
        use aws_lc_rs::encoding::{AsBigEndian as _, EcPrivateKeyBin};
        let device = agreement::PrivateKey::generate(&agreement::ECDH_P256).map_err(|e| anyhow::anyhow!("{e}"))?;
        let public = device.compute_public_key().map_err(|e| anyhow::anyhow!("{e}"))?.as_ref().to_vec();
        let private: EcPrivateKeyBin<'static> = device.as_be_bytes().map_err(|e| anyhow::anyhow!("{e}"))?;
        let auth = [7u8; 16];
        let text = br#"{"title":"Mantle","body":"Asks: which database?"}"#;
        let one = encrypt(text, &public, &auth)?;
        let two = encrypt(text, &public, &auth)?;
        assert_ne!(one, two, "a fresh salt and key pair each time");
        assert_eq!(decrypt(&one, private.as_ref(), &public, &auth)?, text);
        assert_eq!(decrypt(&one, private.as_ref(), &public, &[8u8; 16]), Err(Error::NotOurs), "another device's secret reads nothing");
        assert_eq!(encrypt(text, &public, auth.get(..15).unwrap_or_default()), Err(Error::BadAuth));
        assert_eq!(encrypt(text, public.get(..64).unwrap_or_default(), &auth), Err(Error::BadKey));
        assert_eq!(encrypt(&vec![b'x'; MAX_PLAINTEXT + 1], &public, &auth), Err(Error::TooBig(MAX_PLAINTEXT + 1)));
        assert_eq!(encrypt(&vec![b'x'; MAX_PLAINTEXT], &public, &auth)?.len(), 4096, "the most fills 4096 bytes exactly");
        Ok(())
    }
}

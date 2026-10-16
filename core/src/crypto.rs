//! UKEY2 key agreement + the SecureMessage (AES-256-CBC + HMAC-SHA256) channel.

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use p256::{ecdh::EphemeralSecret, EncodedPoint, PublicKey};
use prost::Message;
use sha2::{Digest, Sha256, Sha512};

use crate::proto::{securegcm as gcm, securemessage as sm};
use crate::Error;

pub const NEXT_PROTOCOL: &str = "AES_256_CBC-HMAC_SHA256";

pub struct Keys {
    enc: [u8; 32],
    dec: [u8; 32],
    send_hmac: [u8; 32],
    recv_hmac: [u8; 32],
}

fn hkdf32(ikm: &[u8], salt: &[u8], info: &[u8]) -> [u8; 32] {
    let mut out = [0; 32];
    Hkdf::<Sha256>::new(Some(salt), ikm)
        .expand(info, &mut out)
        .unwrap();
    out
}

/// Java peers parse coordinates as signed BigIntegers: prefix 0x00 when the high bit is set.
fn signed(b: &[u8]) -> Vec<u8> {
    if b[0] & 0x80 != 0 {
        [&[0][..], b].concat()
    } else {
        b.to_vec()
    }
}

fn fixed32(b: &[u8]) -> Result<[u8; 32], Error> {
    let start = b.len().saturating_sub(32);
    if b[..start].iter().any(|&c| c != 0) {
        return Err(Error::Protocol("bad EC coordinate"));
    }
    let mut out = [0; 32];
    out[32 - (b.len() - start)..].copy_from_slice(&b[start..]);
    Ok(out)
}

pub fn encode_public_key(pk: &PublicKey) -> Vec<u8> {
    let p = pk.to_encoded_point(false);
    sm::GenericPublicKey {
        r#type: sm::PublicKeyType::EcP256 as i32,
        ec_p256_public_key: Some(sm::EcP256PublicKey {
            x: signed(p.x().unwrap()),
            y: signed(p.y().unwrap()),
        }),
        ..Default::default()
    }
    .encode_to_vec()
}

pub fn decode_public_key(bytes: &[u8]) -> Result<PublicKey, Error> {
    let k = sm::GenericPublicKey::decode(bytes)?
        .ec_p256_public_key
        .ok_or(Error::Protocol("missing P-256 key"))?;
    let p = EncodedPoint::from_affine_coordinates(
        &fixed32(&k.x)?.into(),
        &fixed32(&k.y)?.into(),
        false,
    );
    Option::from(PublicKey::from_encoded_point(&p)).ok_or(Error::Protocol("invalid P-256 key"))
}

pub fn sha512(b: &[u8]) -> Vec<u8> {
    Sha512::digest(b).to_vec()
}

/// Finish UKEY2: returns the channel keys and the 4-digit PIN shown on both screens.
pub fn derive(
    secret: &EphemeralSecret,
    peer: &PublicKey,
    client_init: &[u8],
    server_init: &[u8],
    is_server: bool,
) -> (Keys, String) {
    let dhs = Sha256::digest(secret.diffie_hellman(peer).raw_secret_bytes());
    let info = [client_init, server_init].concat();
    let auth = hkdf32(&dhs, b"UKEY2 v1 auth", &info);
    let next = hkdf32(&dhs, b"UKEY2 v1 next", &info);

    let d2d = Sha256::digest(b"D2D");
    let client = hkdf32(&next, &d2d, b"client");
    let server = hkdf32(&next, &d2d, b"server");
    let salt = Sha256::digest(b"SecureMessage");
    let (c_enc, c_sig) = (
        hkdf32(&client, &salt, b"ENC:2"),
        hkdf32(&client, &salt, b"SIG:1"),
    );
    let (s_enc, s_sig) = (
        hkdf32(&server, &salt, b"ENC:2"),
        hkdf32(&server, &salt, b"SIG:1"),
    );

    let keys = if is_server {
        Keys {
            enc: s_enc,
            send_hmac: s_sig,
            dec: c_enc,
            recv_hmac: c_sig,
        }
    } else {
        Keys {
            enc: c_enc,
            send_hmac: c_sig,
            dec: s_enc,
            recv_hmac: s_sig,
        }
    };
    (keys, pin(&auth))
}

/// Nearby Share's PIN algorithm (not the one from the UKEY2 spec).
fn pin(auth: &[u8]) -> String {
    let (mut hash, mut mul) = (0i32, 1i32);
    for &b in auth {
        hash = (hash + (b as i8 as i32) * mul) % 9973;
        mul = (mul * 31) % 9973;
    }
    format!("{:04}", hash.abs())
}

impl Keys {
    pub fn seal(&self, seq: i32, frame: Vec<u8>) -> Vec<u8> {
        let d2d = gcm::DeviceToDeviceMessage {
            message: Some(frame),
            sequence_number: Some(seq),
        }
        .encode_to_vec();
        let iv: [u8; 16] = rand::random();
        let body = cbc::Encryptor::<aes::Aes256>::new(&self.enc.into(), &iv.into())
            .encrypt_padded_vec_mut::<Pkcs7>(&d2d);
        let header_and_body = sm::HeaderAndBody {
            header: sm::Header {
                signature_scheme: sm::SigScheme::HmacSha256 as i32,
                encryption_scheme: sm::EncScheme::Aes256Cbc as i32,
                iv: Some(iv.to_vec()),
                public_metadata: Some(
                    gcm::GcmMetadata {
                        r#type: gcm::Type::DeviceToDeviceMessage as i32,
                        version: Some(1),
                    }
                    .encode_to_vec(),
                ),
                ..Default::default()
            },
            body,
        }
        .encode_to_vec();
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.send_hmac).unwrap();
        mac.update(&header_and_body);
        sm::SecureMessage {
            header_and_body,
            signature: mac.finalize().into_bytes().to_vec(),
        }
        .encode_to_vec()
    }

    pub fn open(&self, expected_seq: i32, data: &[u8]) -> Result<Vec<u8>, Error> {
        let msg = sm::SecureMessage::decode(data)?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.recv_hmac).unwrap();
        mac.update(&msg.header_and_body);
        mac.verify_slice(&msg.signature)
            .map_err(|_| Error::Protocol("bad HMAC"))?;
        let hb = sm::HeaderAndBody::decode(&*msg.header_and_body)?;
        let iv: [u8; 16] = hb
            .header
            .iv
            .as_deref()
            .and_then(|v| v.try_into().ok())
            .ok_or(Error::Protocol("bad IV"))?;
        let plain = cbc::Decryptor::<aes::Aes256>::new(&self.dec.into(), &iv.into())
            .decrypt_padded_vec_mut::<Pkcs7>(&hb.body)
            .map_err(|_| Error::Protocol("bad padding"))?;
        let d2d = gcm::DeviceToDeviceMessage::decode(&*plain)?;
        if d2d.sequence_number != Some(expected_seq) {
            return Err(Error::Protocol("bad sequence number"));
        }
        d2d.message.ok_or(Error::Protocol("empty secure message"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coords_and_pin() {
        assert_eq!(signed(&[0x80, 1]), vec![0, 0x80, 1]);
        assert_eq!(fixed32(&[0; 33]).unwrap(), [0; 32]);
        assert!(fixed32(&[[1].as_slice(), &[0; 32]].concat()).is_err());
        assert_eq!(fixed32(&[7]).unwrap()[31], 7);
        assert_eq!(pin(&[]), "0000");
        assert_eq!(pin(&[0xff]), "0001"); // -1 -> abs
    }
}

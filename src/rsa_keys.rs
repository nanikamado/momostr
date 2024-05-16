use once_cell::sync::Lazy;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::{RsaPrivateKey, RsaPublicKey};
use sigh::Key;

static RSA_PRIVATE_KEY_STRING: &str = env!("RSA_PRIVATE_KEY");

pub static RSA_PRIVATE_KEY: Lazy<RsaPrivateKey> =
    Lazy::new(|| RsaPrivateKey::from_pkcs8_pem(RSA_PRIVATE_KEY_STRING).unwrap());

pub static RSA_PRIVATE_KEY_FOR_SIGH: Lazy<sigh::PrivateKey> = Lazy::new(|| {
    let s = RSA_PRIVATE_KEY.to_pkcs8_pem(LineEnding::default()).unwrap();
    sigh::PrivateKey::from_pem(s.as_bytes()).unwrap()
});

pub static RSA_PUBLIC_KEY: Lazy<RsaPublicKey> = Lazy::new(|| RsaPublicKey::from(&*RSA_PRIVATE_KEY));
pub static RSA_PUBLIC_KEY_STRING: Lazy<String> = Lazy::new(|| {
    RSA_PUBLIC_KEY
        .to_public_key_pem(LineEnding::default())
        .unwrap()
});

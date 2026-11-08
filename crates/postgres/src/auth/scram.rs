//! SCRAM-SHA-256 (RFC 5802 / RFC 7677) SASL authentication. We hold the plaintext
//! password (POSTGRES_PASSWORD), so on each login we pick a fresh salt/iteration
//! count, derive the SCRAM keys, and verify the client's proof. Channel binding
//! (SCRAM-SHA-256-PLUS) is not offered.

use crate::error::{Result, errf};
use crate::protocol::{Credential, Protocol};
use crate::session::new_reader;
use crate::wire::{MSG_AUTHENTICATION, MSG_PASSWORD_MESSAGE, WireConn, append_cstring, i32_be};
use base64::Engine;
use base64::alphabet;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, NO_PAD, STANDARD};
use hmac::{Hmac, Mac};
use overlite_core::SqlState;
use overlite_core::text::quoted;
use rand::RngCore;
use sha2::{Digest, Sha256};

const SCRAM_ITERATIONS: i64 = 4096;

const SHA256_SIZE: usize = 32;

/// Standard base64 where trailing bits are not checked.
const GO_STD: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true),
);

/// Decodes standard base64, skipping CR and LF.
fn std_decode(s: &str) -> Option<Vec<u8>> {
    if s.contains(['\r', '\n']) {
        let clean: String = s.chars().filter(|c| *c != '\r' && *c != '\n').collect();
        return GO_STD.decode(clean).ok();
    }
    GO_STD.decode(s).ok()
}

/// The SCRAM keys derived from a password (the form Postgres stores in
/// rolpassword): enough to authenticate without the plaintext.
#[derive(Clone, Debug, Default)]
pub(crate) struct ScramVerifier {
    iter: i64,
    salt: Vec<u8>,
    stored_key: Vec<u8>,
    server_key: Vec<u8>,
}

fn crypto_rand(buf: &mut [u8]) -> Result<()> {
    rand::rngs::OsRng
        .try_fill_bytes(buf)
        .map_err(|e| errf!(SqlState::INTERNAL_ERROR, "{}", e))
}

/// Derives a fresh verifier for password and encodes it as Postgres does:
/// SCRAM-SHA-256$<iter>:<salt>$<StoredKey>:<ServerKey> (base64).
pub(crate) fn build_scram_verifier(password: &str) -> Result<String> {
    let mut salt = [0u8; 16];
    crypto_rand(&mut salt)?;
    let v = derive_verifier(password, &salt, SCRAM_ITERATIONS);
    Ok(format!(
        "SCRAM-SHA-256${}:{}${}:{}",
        v.iter,
        STANDARD.encode(&v.salt),
        STANDARD.encode(&v.stored_key),
        STANDARD.encode(&v.server_key)
    ))
}

/// Computes the SCRAM keys for a password with a given salt/iter.
fn derive_verifier(password: &str, salt: &[u8], iter: i64) -> ScramVerifier {
    let salted = pbkdf2_sha256(password.as_bytes(), salt, iter, SHA256_SIZE);
    let client_key = hmac_sha256(&salted, b"Client Key");
    let stored_key = Sha256::digest(&client_key);
    ScramVerifier {
        iter,
        salt: salt.to_vec(),
        stored_key: stored_key.to_vec(),
        server_key: hmac_sha256(&salted, b"Server Key"),
    }
}

/// Decodes a stored "SCRAM-SHA-256$..." verifier string.
pub(crate) fn parse_scram_verifier(s: &str) -> Option<ScramVerifier> {
    let rest = s.strip_prefix("SCRAM-SHA-256$")?;
    let (salt_part, key_part) = rest.split_once('$')?;
    let (iter_str, salt_b64) = salt_part.split_once(':')?;
    let (stored_b64, server_b64) = key_part.split_once(':')?;
    let iter: i64 = iter_str.parse().ok()?;
    let salt = std_decode(salt_b64);
    let stored = std_decode(stored_b64);
    let server = std_decode(server_b64);
    Some(ScramVerifier {
        iter,
        salt: salt?,
        stored_key: stored?,
        server_key: server?,
    })
}

impl ScramVerifier {
    /// Whether password reproduces this verifier's StoredKey (used for cleartext
    /// auth against a stored verifier).
    pub(crate) fn verifies(&self, password: &str) -> bool {
        let got = derive_verifier(password, &self.salt, self.iter);
        hmac_equal(&got.stored_key, &self.stored_key)
    }
}

impl Protocol {
    /// Runs the four-message SASL exchange against cred, Ok when the client
    /// proves knowledge of the password. The salt/iteration/keys come from a
    /// stored verifier when present, otherwise they are derived from the known
    /// plaintext with a fresh salt.
    pub(crate) fn auth_scram(&self, c: &mut WireConn, _user: &str, cred: &Credential) -> Result<()> {
        // AuthenticationSASL (10): advertise the one mechanism (list ends with an
        // extra NUL).
        let mut msg = i32_be(10).to_vec();
        append_cstring(&mut msg, "SCRAM-SHA-256");
        msg.push(0);
        c.send(MSG_AUTHENTICATION, &msg)?;
        c.flush()?;

        // SASLInitialResponse: mechanism, int32 length, client-first-message.
        let (typ, body) = c.read_auth_message()?;
        if typ != MSG_PASSWORD_MESSAGE {
            return Err(errf!(
                SqlState::PROTOCOL_VIOLATION,
                "expected SASL initial response, got {:?}",
                quoted((typ as char).to_string())
            ));
        }
        let mut r = new_reader(&body);
        if r.cstring() != "SCRAM-SHA-256" {
            return Err(errf!(SqlState::PROTOCOL_VIOLATION, "unsupported SASL mechanism"));
        }
        let n = r.int32();
        let client_first = String::from_utf8_lossy(r.bytes(n)).into_owned();
        if r.err {
            return Err(errf!(SqlState::PROTOCOL_VIOLATION, "malformed SASL initial response"));
        }
        let client_first_bare = scram_bare(&client_first);
        let client_nonce = scram_attr(client_first_bare, "r");
        if client_nonce.is_empty() {
            return Err(errf!(SqlState::PROTOCOL_VIOLATION, "missing client nonce"));
        }

        // The verifier: use the role's stored keys, or derive from the plaintext.
        let derived;
        let v: &ScramVerifier = if let Some(v) = &cred.scram {
            v
        } else {
            let mut salt = [0u8; 16];
            crypto_rand(&mut salt)?;
            derived = derive_verifier(&cred.plaintext, &salt, SCRAM_ITERATIONS);
            &derived
        };

        // AuthenticationSASLContinue (11): server-first-message.
        let server_nonce = random_nonce()?;
        let nonce = format!("{client_nonce}{server_nonce}");
        let server_first = format!("r={},s={},i={}", nonce, STANDARD.encode(&v.salt), v.iter);
        let mut msg = i32_be(11).to_vec();
        msg.extend_from_slice(server_first.as_bytes());
        c.send(MSG_AUTHENTICATION, &msg)?;
        c.flush()?;

        // SASLResponse: client-final-message.
        let (typ, body) = c.read_auth_message()?;
        if typ != MSG_PASSWORD_MESSAGE {
            return Err(errf!(
                SqlState::PROTOCOL_VIOLATION,
                "expected SASL response, got {:?}",
                quoted((typ as char).to_string())
            ));
        }
        let client_final = String::from_utf8_lossy(&body).into_owned();
        if scram_attr(&client_final, "r") != nonce {
            let _ = self.auth_failed(c);
            return Err(errf!(SqlState::PROTOCOL_VIOLATION, "scram nonce mismatch"));
        }
        let Some(proof) = std_decode(scram_attr(&client_final, "p")) else {
            return Err(errf!(SqlState::PROTOCOL_VIOLATION, "bad client proof"));
        };

        // Verify the client proof against the StoredKey.
        let auth_message = format!(
            "{},{},{}",
            client_first_bare,
            server_first,
            scram_without_proof(&client_final)
        );
        let client_sig = hmac_sha256(&v.stored_key, auth_message.as_bytes());
        if proof.len() != client_sig.len() {
            return self.auth_failed(c);
        }
        // ClientKey = ClientProof XOR ClientSignature; it must hash to StoredKey.
        let recovered: Vec<u8> = proof.iter().zip(&client_sig).map(|(a, b)| a ^ b).collect();
        let got = Sha256::digest(&recovered);
        if !hmac_equal(&got, &v.stored_key) {
            return self.auth_failed(c);
        }

        // AuthenticationSASLFinal (12): send the server signature.
        let server_sig = hmac_sha256(&v.server_key, auth_message.as_bytes());
        let final_msg = format!("v={}", STANDARD.encode(&server_sig));
        let mut msg = i32_be(12).to_vec();
        msg.extend_from_slice(final_msg.as_bytes());
        c.send(MSG_AUTHENTICATION, &msg)?;
        c.flush()
    }
}

/// Strips the GS2 header ("n,," / "y,," / "p=...,,") from the
/// client-first-message, leaving the bare "n=user,r=nonce".
fn scram_bare(client_first: &str) -> &str {
    let Some(i1) = client_first.find(',') else {
        return client_first;
    };
    let Some(i2) = client_first[i1 + 1..].find(',') else {
        return client_first;
    };
    &client_first[i1 + 1 + i2 + 1..]
}

/// The client-final-message with the trailing ",p=..." removed (the AuthMessage
/// covers the message up to but not including the proof).
fn scram_without_proof(client_final: &str) -> &str {
    match client_final.rfind(",p=") {
        Some(i) => &client_final[..i],
        None => client_final,
    }
}

/// The value of the "key=value" attribute of a comma-separated SCRAM message.
fn scram_attr<'a>(msg: &'a str, key: &str) -> &'a str {
    for part in msg.split(',') {
        if let Some(rest) = part.strip_prefix(key)
            && let Some(value) = rest.strip_prefix('=')
        {
            return value;
        }
    }
    ""
}

/// A base64 printable nonce (SCRAM nonces must avoid commas).
fn random_nonce() -> Result<String> {
    const RAW_STD: GeneralPurpose = GeneralPurpose::new(&alphabet::STANDARD, NO_PAD);
    let mut b = [0u8; 18];
    crypto_rand(&mut b)?;
    Ok(RAW_STD.encode(b))
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    #[allow(clippy::expect_used)] // new_from_slice fails only for fixed-key MACs
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes a key of any length");
    m.update(msg);
    m.finalize().into_bytes().to_vec()
}

/// Constant-time comparison.
fn hmac_equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// PBKDF2 with HMAC-SHA-256 (RFC 2898), enough for SCRAM's key derivation.
fn pbkdf2_sha256(password: &[u8], salt: &[u8], iter: i64, key_len: usize) -> Vec<u8> {
    let h_len = SHA256_SIZE;
    let blocks = key_len.div_ceil(h_len);
    let mut dk: Vec<u8> = Vec::new();
    for block in 1..=blocks {
        let idx = (block as u32).to_be_bytes();
        let mut input = salt.to_vec();
        input.extend_from_slice(&idx);
        let mut u = hmac_sha256(password, &input);
        let mut t = u.clone();
        let mut i = 1;
        while i < iter {
            u = hmac_sha256(password, &u);
            for j in 0..t.len() {
                t[j] ^= u[j];
            }
            i += 1;
        }
        dk.extend_from_slice(&t);
    }
    dk.truncate(key_len);
    dk
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // Checks the SCRAM verifier build/parse: deriving from the right password
    // reproduces the stored keys, a wrong one does not.
    #[test]
    fn verifier_round_trip() {
        let v = build_scram_verifier("s3cr3t").unwrap();

        let parsed = parse_scram_verifier(&v).unwrap();
        assert!(parsed.verifies("s3cr3t"), "correct password verifies");
        assert!(!parsed.verifies("wrong"), "wrong password does not");

        assert!(parse_scram_verifier("not-a-verifier").is_none());
    }
}

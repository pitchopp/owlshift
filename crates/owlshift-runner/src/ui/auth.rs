//! Who may drive the web UI (OWL-467, architecture section 5, "Who may
//! drive it"): every MAC is HMAC-SHA256, each use behind its own label, and
//! every check is constant-time.
//!
//! - *The proof*, for `owlshift ui` and `owlshift doctor`: the listener
//!   answers a 32-byte challenge with `HMAC_token("owlshift-ui proof\n" ‖
//!   challenge)`, which only a holder of the token makes.
//! - *The one-time code* `owlshift ui` mints with the token alone:
//!   `<nonce>.<ms since the epoch>.<HMAC_token("owlshift-ui login\n" ‖ nonce
//!   ‖ ms as u64 big-endian)>`, the nonce 16 random bytes, each part in
//!   lowercase hexadecimal but the time. It is refused older than
//!   [`CODE_LIFETIME_MS`], more than [`CODE_SKEW_MS`] in the future, older
//!   than the listener's own start, or used already ([`UsedCodes`]).
//! - *The session*: an id of 16 random bytes and its secret
//!   `HMAC_token("owlshift-ui session\n" ‖ run secret ‖ id)`, sent as
//!   `<id>.<secret>`. Nothing about it is stored: it holds while the token
//!   and the daemon's run do, so a restart of the UI under the same daemon
//!   keeps it, and a new daemon run, an upgrade's included, or a new token
//!   ends it.
//!
//! No proof, code or session holds the token or the run secret.

use std::collections::HashMap;
use std::fmt;
use std::io;

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

use super::token::Token;

type HmacSha256 = Hmac<Sha256>;

const PROOF_LABEL: &[u8] = b"owlshift-ui proof\n";
const LOGIN_LABEL: &[u8] = b"owlshift-ui login\n";
const SESSION_LABEL: &[u8] = b"owlshift-ui session\n";
const FINGERPRINT_LABEL: &[u8] = b"owlshift-ui run secret fingerprint\n";

/// The bytes of a proof's challenge.
pub const CHALLENGE_LEN: usize = 32;

/// How long a one-time code may be used after it was minted: 60 seconds.
pub const CODE_LIFETIME_MS: u64 = 60_000;

/// How far in the future a code's time may lie, for a clock a little
/// ahead: 5 seconds.
pub const CODE_SKEW_MS: u64 = 5_000;

const NONCE_LEN: usize = 16;
const SESSION_ID_LEN: usize = 16;
const MAC_LEN: usize = 32;

/// `bytes` in lowercase hexadecimal.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// The bytes `text` spells in lowercase hexadecimal, `None` for anything
/// else.
pub fn from_hex(text: &str) -> Option<Vec<u8>> {
    fn digit(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        }
    }
    let (pairs, rest) = text.as_bytes().as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    pairs
        .iter()
        .map(|[high, low]| Some((digit(*high)? << 4) | digit(*low)?))
        .collect()
}

/// The `N` bytes `text` spells in lowercase hexadecimal, `None` for any
/// other text or length.
pub fn from_hex_array<const N: usize>(text: &str) -> Option<[u8; N]> {
    from_hex(text)?.try_into().ok()
}

/// `N` random bytes from the operating system.
pub fn random<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes)
        .map_err(|error| io::Error::other(format!("no random bytes: {error}")))?;
    Ok(bytes)
}

/// The daemon's run secret: 32 random bytes made once at each start of the
/// daemon, kept in memory only, and handed to each UI it starts on a
/// descriptor. Its `Debug` prints nothing of it.
#[derive(Clone, PartialEq, Eq)]
pub struct RunSecret([u8; 32]);

impl RunSecret {
    /// A new run secret, from the operating system's random bytes.
    pub fn new() -> io::Result<Self> {
        random().map(Self)
    }

    /// As it is handed over: 64 lowercase hexadecimal digits.
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }

    /// The run secret `text` spells, 64 lowercase hexadecimal digits.
    pub fn from_hex(text: &str) -> Option<Self> {
        from_hex_array(text).map(Self)
    }

    /// A digest that tells two run secrets apart without revealing either,
    /// for tests and the test bench: the SHA-256 of a label and the bytes,
    /// in hexadecimal.
    pub fn fingerprint(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(FINGERPRINT_LABEL);
        hash.update(self.0);
        hex(&hash.finalize())
    }
}

impl fmt::Debug for RunSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RunSecret(..)")
    }
}

fn mac(token: &Token, parts: &[&[u8]]) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(token.as_bytes()).expect("HMAC takes any key");
    for part in parts {
        mac.update(part);
    }
    mac
}

/// Whether `expected` in hexadecimal is the MAC of `parts`, compared in
/// constant time.
fn verifies(token: &Token, parts: &[&[u8]], expected: &str) -> bool {
    match from_hex_array::<MAC_LEN>(expected) {
        Some(bytes) => mac(token, parts).verify_slice(&bytes).is_ok(),
        None => false,
    }
}

/// The listener's answer to `challenge`, in hexadecimal.
pub fn prove(token: &Token, challenge: &[u8; CHALLENGE_LEN]) -> String {
    hex(&mac(token, &[PROOF_LABEL, challenge])
        .finalize()
        .into_bytes())
}

/// Whether `proof` answers `challenge` under `token`.
pub fn check_proof(token: &Token, challenge: &[u8; CHALLENGE_LEN], proof: &str) -> bool {
    verifies(token, &[PROOF_LABEL, challenge], proof)
}

/// A one-time code minted at `now_ms`, in milliseconds since the Unix
/// epoch.
pub fn mint_code(token: &Token, now_ms: u64) -> io::Result<String> {
    let nonce: [u8; NONCE_LEN] = random()?;
    let tag = mac(token, &[LOGIN_LABEL, &nonce, &now_ms.to_be_bytes()]);
    Ok(format!(
        "{}.{now_ms}.{}",
        hex(&nonce),
        hex(&tag.finalize().into_bytes())
    ))
}

/// Why a code was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeRefused {
    /// It is not a code.
    Malformed,
    /// Its MAC does not hold under the token.
    NotMinted,
    /// It is older than [`CODE_LIFETIME_MS`].
    Expired,
    /// It lies more than [`CODE_SKEW_MS`] in the future.
    Future,
    /// It was minted before this listener started.
    BeforeStart,
    /// It was used already.
    Used,
}

/// The codes a listener has taken, by nonce, until they expire; and when
/// the listener started, so a restart forgets no code it took.
#[derive(Debug)]
pub struct UsedCodes {
    started_ms: u64,
    used: HashMap<[u8; NONCE_LEN], u64>,
}

impl UsedCodes {
    /// For a listener that started at `started_ms`.
    pub fn new(started_ms: u64) -> Self {
        Self {
            started_ms,
            used: HashMap::new(),
        }
    }

    /// Takes `code` at `now_ms`: it holds once, under `token`, within its
    /// lifetime and after the listener's start.
    pub fn take(&mut self, token: &Token, code: &str, now_ms: u64) -> Result<(), CodeRefused> {
        self.used.retain(|_, expiry| *expiry >= now_ms);
        let mut parts = code.split('.');
        let (Some(nonce), Some(minted), Some(tag), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(CodeRefused::Malformed);
        };
        let nonce: [u8; NONCE_LEN] = from_hex_array(nonce).ok_or(CodeRefused::Malformed)?;
        if minted.is_empty() || !minted.bytes().all(|b| b.is_ascii_digit()) {
            return Err(CodeRefused::Malformed);
        }
        let minted: u64 = minted.parse().map_err(|_| CodeRefused::Malformed)?;
        if !verifies(token, &[LOGIN_LABEL, &nonce, &minted.to_be_bytes()], tag) {
            return Err(CodeRefused::NotMinted);
        }
        if minted > now_ms.saturating_add(CODE_SKEW_MS) {
            return Err(CodeRefused::Future);
        }
        if now_ms.saturating_sub(minted) > CODE_LIFETIME_MS {
            return Err(CodeRefused::Expired);
        }
        if minted < self.started_ms {
            return Err(CodeRefused::BeforeStart);
        }
        if self.used.contains_key(&nonce) {
            return Err(CodeRefused::Used);
        }
        self.used
            .insert(nonce, minted.saturating_add(CODE_LIFETIME_MS));
        Ok(())
    }
}

/// A session the login hands out: its id and its secret, in hexadecimal.
#[derive(Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub secret: String,
}

impl Session {
    /// The value of the header the page sends it in: `<id>.<secret>`.
    pub fn header(&self) -> String {
        format!("{}.{}", self.id, self.secret)
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session").field("id", &self.id).finish()
    }
}

/// A new session under `token` and the daemon's run secret.
pub fn mint_session(token: &Token, run: &RunSecret) -> io::Result<Session> {
    let id: [u8; SESSION_ID_LEN] = random()?;
    let secret = mac(token, &[SESSION_LABEL, &run.0, &id]).finalize();
    Ok(Session {
        id: hex(&id),
        secret: hex(&secret.into_bytes()),
    })
}

/// Whether `value`, `<id>.<secret>`, is a session under `token` and the
/// daemon's run secret.
pub fn check_session(token: &Token, run: &RunSecret, value: &str) -> bool {
    let Some((id, secret)) = value.split_once('.') else {
        return false;
    };
    let Some(id) = from_hex_array::<SESSION_ID_LEN>(id) else {
        return false;
    };
    verifies(token, &[SESSION_LABEL, &run.0, &id], secret)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(byte: u8) -> Token {
        Token::from_bytes([byte; 32])
    }

    const NOW: u64 = 1_793_000_000_000;

    #[test]
    fn hex_round_trips_and_refuses_anything_else() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        assert_eq!(from_hex("000fa5ff").unwrap(), [0x00, 0x0f, 0xa5, 0xff]);
        for bad in ["0", "0g", "AB", "a b"] {
            assert!(from_hex(bad).is_none(), "{bad}");
        }
        assert!(from_hex_array::<2>("aabbcc").is_none());
    }

    #[test]
    fn a_proof_checks_under_its_token_only() {
        let challenge = [7; CHALLENGE_LEN];
        let proof = prove(&token(1), &challenge);
        assert!(check_proof(&token(1), &challenge, &proof));
        assert!(!check_proof(&token(2), &challenge, &proof));
        assert!(!check_proof(&token(1), &[8; CHALLENGE_LEN], &proof));
        assert!(!check_proof(&token(1), &challenge, "00"));
    }

    #[test]
    fn a_code_is_taken_once_within_its_lifetime_and_after_the_start() {
        let mut used = UsedCodes::new(NOW - 1_000);
        let code = mint_code(&token(1), NOW).unwrap();
        assert_eq!(
            used.take(&token(2), &code, NOW),
            Err(CodeRefused::NotMinted)
        );
        assert_eq!(used.take(&token(1), &code, NOW + 10), Ok(()));
        assert_eq!(
            used.take(&token(1), &code, NOW + 20),
            Err(CodeRefused::Used)
        );

        let old = mint_code(&token(1), NOW).unwrap();
        assert_eq!(
            UsedCodes::new(0).take(&token(1), &old, NOW + 60_001),
            Err(CodeRefused::Expired)
        );
        assert_eq!(
            UsedCodes::new(0).take(&token(1), &old, NOW + 60_000),
            Ok(())
        );
        let ahead = mint_code(&token(1), NOW + 5_001).unwrap();
        assert_eq!(
            UsedCodes::new(0).take(&token(1), &ahead, NOW),
            Err(CodeRefused::Future)
        );
        let before = mint_code(&token(1), NOW).unwrap();
        assert_eq!(
            UsedCodes::new(NOW + 1).take(&token(1), &before, NOW + 2),
            Err(CodeRefused::BeforeStart)
        );
        for bad in ["", "x", "a.b.c", "a.1.b.c"] {
            assert_eq!(
                UsedCodes::new(0).take(&token(1), bad, NOW),
                Err(CodeRefused::Malformed)
            );
        }
        let (nonce, rest) = before.split_once('.').unwrap();
        let (_, tag) = rest.split_once('.').unwrap();
        let shifted = format!("{nonce}.{}.{tag}", NOW + 1);
        assert_eq!(
            UsedCodes::new(0).take(&token(1), &shifted, NOW),
            Err(CodeRefused::NotMinted),
            "the time is under the MAC"
        );
    }

    #[test]
    fn a_used_code_is_forgotten_once_it_expired() {
        let mut used = UsedCodes::new(0);
        let code = mint_code(&token(1), NOW).unwrap();
        used.take(&token(1), &code, NOW).unwrap();
        let later = mint_code(&token(1), NOW + 120_000).unwrap();
        used.take(&token(1), &later, NOW + 120_000).unwrap();
        assert_eq!(used.used.len(), 1);
    }

    #[test]
    fn a_session_checks_under_its_token_and_run_secret() {
        let run = RunSecret::new().unwrap();
        let session = mint_session(&token(1), &run).unwrap();
        assert!(check_session(&token(1), &run, &session.header()));
        let made_up = format!("{}.{}", "0".repeat(32), "0".repeat(64));
        assert!(!check_session(&token(1), &run, &made_up));
        assert!(!check_session(&token(1), &run, "nonsense"));
        assert!(!check_session(&token(2), &run, &session.header()));
    }

    #[test]
    fn a_session_of_an_earlier_daemon_run_is_refused() {
        let first = RunSecret::new().unwrap();
        let session = mint_session(&token(1), &first).unwrap();
        let second = RunSecret::new().unwrap();
        assert_ne!(first, second);
        assert!(!check_session(&token(1), &second, &session.header()));
        assert_ne!(first.fingerprint(), second.fingerprint());
    }

    #[test]
    fn a_session_minted_before_a_new_token_is_refused() {
        let data = tempfile::tempdir().unwrap();
        let old = crate::ui::token::ensure_token(data.path()).unwrap();
        let run = RunSecret::new().unwrap();
        let session = mint_session(&old, &run).unwrap();
        let new = crate::ui::token::new_token(data.path()).unwrap();
        assert!(!check_session(&new, &run, &session.header()));
    }

    #[test]
    fn a_run_secret_round_trips_its_hex_only() {
        let run = RunSecret::new().unwrap();
        assert_eq!(RunSecret::from_hex(&run.to_hex()).unwrap(), run);
        let hex = run.to_hex();
        assert!(RunSecret::from_hex(&hex[..63]).is_none());
        assert!(RunSecret::from_hex(&format!("{hex}0")).is_none());
        assert!(RunSecret::from_hex(&"g".repeat(64)).is_none());
        assert_eq!(format!("{run:?}"), "RunSecret(..)");
    }

    #[test]
    fn nothing_minted_holds_the_token_or_the_run_secret() {
        let token = Token::from_bytes(random().unwrap());
        let run = RunSecret::new().unwrap();
        let session = mint_session(&token, &run).unwrap();
        let minted = [
            prove(&token, &[3; CHALLENGE_LEN]),
            mint_code(&token, NOW).unwrap(),
            session.header(),
            format!("{session:?}"),
            run.fingerprint(),
        ];
        for text in minted {
            assert!(!text.contains(&token.to_hex()), "{text}");
            assert!(!text.contains(&run.to_hex()), "{text}");
            assert!(
                !text
                    .as_bytes()
                    .windows(token.as_bytes().len())
                    .any(|window| window == token.as_bytes()),
                "{text}"
            );
        }
    }
}

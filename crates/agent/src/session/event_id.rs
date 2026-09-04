use std::fmt;

use sha2::{Digest, Sha256};
use ulid::Ulid;

const MAX_SEQUENCE: u64 = 99_999_999_999_999;
const HASH_DOMAIN: &[u8] = b"zork-agent:event-id:v1\0";

const VERHOEFF_D: [[u8; 10]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
    [1, 2, 3, 4, 0, 6, 7, 8, 9, 5],
    [2, 3, 4, 0, 1, 7, 8, 9, 5, 6],
    [3, 4, 0, 1, 2, 8, 9, 5, 6, 7],
    [4, 0, 1, 2, 3, 9, 5, 6, 7, 8],
    [5, 9, 8, 7, 6, 0, 4, 3, 2, 1],
    [6, 5, 9, 8, 7, 1, 0, 4, 3, 2],
    [7, 6, 5, 9, 8, 2, 1, 0, 4, 3],
    [8, 7, 6, 5, 9, 3, 2, 1, 0, 4],
    [9, 8, 7, 6, 5, 4, 3, 2, 1, 0],
];

const VERHOEFF_P: [[u8; 10]; 8] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
    [1, 5, 7, 6, 2, 8, 3, 0, 9, 4],
    [5, 8, 0, 3, 7, 9, 6, 1, 4, 2],
    [8, 9, 1, 6, 0, 4, 3, 5, 2, 7],
    [9, 4, 5, 3, 1, 2, 6, 8, 7, 0],
    [4, 2, 8, 6, 5, 7, 3, 9, 0, 1],
    [2, 7, 9, 3, 8, 0, 6, 4, 1, 5],
    [7, 0, 4, 6, 9, 1, 3, 2, 5, 8],
];

const VERHOEFF_INVERSE: [u8; 10] = [0, 4, 3, 2, 1, 5, 6, 7, 8, 9];

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EventId(String);

impl EventId {
    pub const MAX_SEQUENCE: u64 = MAX_SEQUENCE;

    pub fn from_sequence(session_id: Ulid, sequence: u64) -> Result<Self, EventIdError> {
        if !(1..=MAX_SEQUENCE).contains(&sequence) {
            return Err(EventIdError::SequenceOutOfRange(sequence));
        }

        let sequence_text = format!("{sequence:014}");
        let verhoeff = verhoeff_digit(sequence_text.as_bytes());
        let session_check = session_check_digit(session_id, sequence);

        let mut encoded = String::with_capacity(16);
        encoded.push_str(&sequence_text);
        encoded.push(char::from(b'0' + verhoeff));
        encoded.push(char::from(b'0' + session_check));
        Ok(Self(encoded))
    }

    pub fn parse(session_id: Ulid, encoded: &str) -> Result<Self, EventIdError> {
        Self::parse_sequence(session_id, encoded)?;
        Ok(Self(encoded.to_owned()))
    }

    pub(crate) fn parse_sequence(session_id: Ulid, encoded: &str) -> Result<u64, EventIdError> {
        let bytes = encoded.as_bytes();
        if bytes.len() != 16 || !bytes.iter().all(u8::is_ascii_digit) {
            return Err(EventIdError::NonCanonical);
        }
        let sequence = bytes[..14]
            .iter()
            .fold(0_u64, |value, digit| value * 10 + u64::from(*digit - b'0'));
        if !(1..=MAX_SEQUENCE).contains(&sequence) {
            return Err(EventIdError::SequenceOutOfRange(sequence));
        }
        if bytes[14] != b'0' + verhoeff_digit(&bytes[..14])
            || bytes[15] != b'0' + session_check_digit(session_id, sequence)
        {
            return Err(EventIdError::ChecksumMismatch);
        }
        Ok(sequence)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn sequence(&self) -> u64 {
        self.0[..14]
            .parse()
            .expect("EventId always contains a validated sequence")
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EventIdError {
    #[error("event sequence {0} is outside 1..={MAX_SEQUENCE}")]
    SequenceOutOfRange(u64),
    #[error("event id must use the canonical 16-digit representation")]
    NonCanonical,
    #[error("event id checksum does not match its session and sequence")]
    ChecksumMismatch,
}

fn verhoeff_digit(digits: &[u8]) -> u8 {
    let mut checksum = 0_usize;
    for (index, digit) in digits.iter().rev().enumerate() {
        let digit = usize::from(*digit - b'0');
        checksum = usize::from(
            VERHOEFF_D[checksum][usize::from(VERHOEFF_P[(index + 1) % VERHOEFF_P.len()][digit])],
        );
    }
    VERHOEFF_INVERSE[checksum]
}

fn session_check_digit(session_id: Ulid, sequence: u64) -> u8 {
    let mut hasher = Sha256::new();
    hasher.update(HASH_DOMAIN);
    hasher.update(session_id.0.to_be_bytes());
    hasher.update(sequence.to_be_bytes());
    hasher.finalize().iter().fold(0_u16, |remainder, byte| {
        (remainder * 256 + u16::from(*byte)) % 10
    }) as u8
}

use std::fmt;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BrokerEpoch([u8; 16]);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrokerEpochParseError {
    InvalidLength,
    InvalidHyphen,
    InvalidHex,
}

impl BrokerEpoch {
    pub fn new() -> Result<Self, getrandom::Error> {
        let mut bytes = [0_u8; 16];
        getrandom::getrandom(&mut bytes)?;
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Ok(Self(bytes))
    }

    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(self) -> [u8; 16] {
        self.0
    }
}

impl fmt::Display for BrokerEpoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for (idx, byte) in self.0.iter().enumerate() {
            if matches!(idx, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            let high = HEX[(byte >> 4) as usize] as char;
            let low = HEX[(byte & 0x0f) as usize] as char;
            f.write_fmt(format_args!("{high}{low}"))?;
        }
        Ok(())
    }
}

impl FromStr for BrokerEpoch {
    type Err = BrokerEpochParseError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        if raw.len() != 36 {
            return Err(BrokerEpochParseError::InvalidLength);
        }

        let mut bytes = [0_u8; 16];
        let mut nibble_idx = 0_usize;

        for (idx, byte) in raw.bytes().enumerate() {
            if matches!(idx, 8 | 13 | 18 | 23) {
                if byte != b'-' {
                    return Err(BrokerEpochParseError::InvalidHyphen);
                }
                continue;
            }
            if byte == b'-' {
                return Err(BrokerEpochParseError::InvalidHyphen);
            }
            let value = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                _ => return Err(BrokerEpochParseError::InvalidHex),
            };
            let slot = nibble_idx / 2;
            if nibble_idx.is_multiple_of(2) {
                bytes[slot] = value << 4;
            } else {
                bytes[slot] |= value;
            }
            nibble_idx += 1;
        }

        if nibble_idx != 32 {
            return Err(BrokerEpochParseError::InvalidLength);
        }

        Ok(Self(bytes))
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct OutputSeq(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputSeqOverflow;

impl OutputSeq {
    pub const fn zero() -> Self {
        Self(0)
    }

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub fn allocate_next(&mut self) -> Result<Self, OutputSeqOverflow> {
        let next = self.0.checked_add(1).ok_or(OutputSeqOverflow)?;
        self.0 = next;
        Ok(Self(next))
    }
}

impl fmt::Display for OutputSeq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

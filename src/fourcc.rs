//! A four-character code, as used for both ISO-BMFF box types and GPMF keys.

use std::fmt;

/// Four bytes, compared and printed as a code rather than as a number.
///
/// GPMF keys are ASCII by convention but not by guarantee, so this stores the
/// raw bytes and only lossily renders them for display.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FourCc(pub [u8; 4]);

impl FourCc {
    pub const ZERO: FourCc = FourCc([0, 0, 0, 0]);

    /// Takes the first four bytes of `s`, zero-padding a short slice.
    pub fn from_slice(s: &[u8]) -> Self {
        let mut b = [0u8; 4];
        let n = s.len().min(4);
        b[..n].copy_from_slice(&s[..n]);
        FourCc(b)
    }

    /// For codes written as literals in `gmax` itself, e.g. `"gpmd"`.
    pub fn from_str_lossy(s: &str) -> Self {
        FourCc::from_slice(s.as_bytes())
    }

    pub fn as_bytes(&self) -> &[u8; 4] {
        &self.0
    }

    /// True when every byte is printable ASCII, i.e. safe to show verbatim.
    pub fn is_printable(&self) -> bool {
        self.0.iter().all(|&b| (0x20..0x7f).contains(&b))
    }
}

impl fmt::Display for FourCc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_printable() {
            for &b in &self.0 {
                write!(f, "{}", b as char)?;
            }
            Ok(())
        } else {
            write!(
                f,
                "0x{:02x}{:02x}{:02x}{:02x}",
                self.0[0], self.0[1], self.0[2], self.0[3]
            )
        }
    }
}

// A code is far more legible as `GPS9` than as a byte array, and it appears in
// almost every diagnostic this tool emits.
impl fmt::Debug for FourCc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl serde::Serialize for FourCc {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

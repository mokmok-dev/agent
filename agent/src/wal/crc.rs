//! `crc32c` (Castagnoli) checksum for the record frame.
//!
//! The lookup table is generated at compile time, so this is a pure index
//! operation with no dependency and nothing for Kani's model checker to
//! struggle with. It is the cheap accidental-corruption check in front of the
//! BLAKE3 chain.

/// The reflected Castagnoli polynomial.
const POLYNOMIAL: u32 = 0x82F6_3B78;

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index: u32 = 0;
    while index < 256 {
        let mut crc = index;
        let mut bit = 0;
        while bit < 8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (POLYNOMIAL & mask);
            bit += 1;
        }
        table[index as usize] = crc;
        index += 1;
    }
    table
}

/// The compile-time `crc32c` lookup table.
const TABLE: [u32; 256] = build_table();

/// Compute the `crc32c` checksum of `bytes`.
#[must_use]
pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        let index = ((crc ^ u32::from(byte)) & 0xFF) as usize;
        crc = (crc >> 8) ^ TABLE[index];
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::crc32c;

    #[test]
    fn matches_the_standard_check_vector() {
        // The CRC-32C check value from the Castagnoli specification.
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn empty_input_is_zero() {
        assert_eq!(crc32c(b""), 0);
    }
}

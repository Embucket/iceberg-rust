//! Iceberg v3 deletion-vector encoding.

use std::io::Read;
use std::ops::BitOrAssign;

use roaring::{RoaringBitmap, RoaringTreemap};

use crate::error::Error;

const MAGIC: [u8; 4] = [0xD1, 0xD3, 0x39, 0x64];
const LENGTH_BYTES: usize = 4;
const MAGIC_BYTES: usize = MAGIC.len();
const CRC_BYTES: usize = 4;
const MIN_BLOB_BYTES: usize = LENGTH_BYTES + MAGIC_BYTES + CRC_BYTES;

/// Compact set of absolute row positions deleted from one Iceberg data file.
#[derive(Debug, Default, PartialEq)]
pub struct DeletionVector {
    positions: RoaringTreemap,
}

impl DeletionVector {
    /// Creates a deletion vector from absolute row positions.
    #[must_use]
    pub fn new(positions: RoaringTreemap) -> Self {
        Self { positions }
    }

    /// Returns whether an absolute row position is deleted.
    #[must_use]
    pub fn contains(&self, position: u64) -> bool {
        self.positions.contains(position)
    }

    /// Returns the number of deleted row positions.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.positions.len()
    }

    /// Returns whether the vector contains no positions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }

    /// Decodes an Iceberg `deletion-vector-v1` Puffin blob.
    ///
    /// The wire layout is `[length: u32 BE][magic][portable roaring64][crc32: u32 BE]`.
    /// The length covers the magic and roaring payload, and the CRC covers the same bytes.
    pub fn decode(blob: &[u8]) -> Result<Self, Error> {
        if blob.len() < MIN_BLOB_BYTES {
            return Err(invalid(format!(
                "deletion-vector-v1 blob is {} bytes, shorter than the {MIN_BLOB_BYTES}-byte minimum",
                blob.len()
            )));
        }

        let body = &blob[LENGTH_BYTES..blob.len() - CRC_BYTES];
        let declared_len = u32::from_be_bytes(blob[..LENGTH_BYTES].try_into()?) as usize;
        if declared_len != body.len() {
            return Err(invalid(format!(
                "deletion-vector-v1 length prefix is {declared_len}, expected {}",
                body.len()
            )));
        }

        let stored_crc = u32::from_be_bytes(blob[blob.len() - CRC_BYTES..].try_into()?);
        let computed_crc = crc32fast::hash(body);
        if stored_crc != computed_crc {
            return Err(invalid(format!(
                "deletion-vector-v1 CRC mismatch: computed {computed_crc:#010x}, stored {stored_crc:#010x}"
            )));
        }

        if body[..MAGIC_BYTES] != MAGIC {
            return Err(invalid(format!(
                "deletion-vector-v1 magic mismatch: {:02x?}, expected {MAGIC:02x?}",
                &body[..MAGIC_BYTES]
            )));
        }

        let positions = decode_roaring_directory(&body[MAGIC_BYTES..])?;
        Ok(Self { positions })
    }
}

impl BitOrAssign for DeletionVector {
    fn bitor_assign(&mut self, rhs: Self) {
        self.positions.bitor_assign(rhs.positions);
    }
}

fn decode_roaring_directory(mut input: &[u8]) -> Result<RoaringTreemap, Error> {
    let bitmap_count = read_u64_le(&mut input)?;
    if bitmap_count > u64::from(u32::MAX) {
        return Err(invalid(format!(
            "deletion-vector-v1 roaring bitmap count {bitmap_count} exceeds the 32-bit key space"
        )));
    }

    let mut bitmaps = Vec::with_capacity(usize::try_from(bitmap_count)?);
    let mut previous_key = None;
    for _ in 0..bitmap_count {
        let key = read_u32_le(&mut input)?;
        if let Some(previous) = previous_key {
            if key <= previous {
                return Err(invalid(format!(
                    "deletion-vector-v1 roaring keys are not strictly ordered: {key} follows {previous}"
                )));
            }
        }
        previous_key = Some(key);
        let bitmap = RoaringBitmap::deserialize_from(&mut input).map_err(Error::from)?;
        bitmaps.push((key, bitmap));
    }
    if !input.is_empty() {
        return Err(invalid(format!(
            "deletion-vector-v1 roaring payload has {} trailing bytes",
            input.len()
        )));
    }
    Ok(RoaringTreemap::from_bitmaps(bitmaps))
}

fn read_u32_le(input: &mut &[u8]) -> Result<u32, Error> {
    let mut bytes = [0; 4];
    input.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64_le(input: &mut &[u8]) -> Result<u64, Error> {
    let mut bytes = [0; 8];
    input.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn invalid(message: String) -> Error {
    Error::InvalidFormat(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(positions: impl IntoIterator<Item = u64>) -> Vec<u8> {
        let positions = positions.into_iter().collect::<RoaringTreemap>();
        let mut roaring = Vec::with_capacity(positions.serialized_size());
        positions.serialize_into(&mut roaring).unwrap();
        let mut body = MAGIC.to_vec();
        body.extend_from_slice(&roaring);

        let mut blob = Vec::with_capacity(LENGTH_BYTES + body.len() + CRC_BYTES);
        blob.extend_from_slice(&u32::try_from(body.len()).unwrap().to_be_bytes());
        blob.extend_from_slice(&body);
        blob.extend_from_slice(&crc32fast::hash(&body).to_be_bytes());
        blob
    }

    #[test]
    fn decodes_positions_across_64_bit_keys() {
        let expected = [0, 5, 1 << 33, (1 << 33) + 7];
        let vector = DeletionVector::decode(&encode(expected)).unwrap();
        assert_eq!(vector.len(), expected.len() as u64);
        for position in expected {
            assert!(vector.contains(position));
        }
        assert!(!vector.contains(6));
    }

    #[test]
    fn rejects_corrupt_crc() {
        let mut blob = encode([1, 2, 3]);
        let last = blob.len() - 1;
        blob[last] ^= 0xff;
        assert!(DeletionVector::decode(&blob)
            .unwrap_err()
            .to_string()
            .contains("CRC mismatch"));
    }

    #[test]
    fn rejects_trailing_roaring_bytes() {
        let mut blob = encode([1]);
        let crc_start = blob.len() - CRC_BYTES;
        blob.insert(crc_start, 0);
        let body_len = blob.len() - LENGTH_BYTES - CRC_BYTES;
        blob[..LENGTH_BYTES].copy_from_slice(&u32::try_from(body_len).unwrap().to_be_bytes());
        let crc = crc32fast::hash(&blob[LENGTH_BYTES..blob.len() - CRC_BYTES]);
        let crc_start = blob.len() - CRC_BYTES;
        blob[crc_start..].copy_from_slice(&crc.to_be_bytes());
        assert!(DeletionVector::decode(&blob)
            .unwrap_err()
            .to_string()
            .contains("trailing bytes"));
    }
}

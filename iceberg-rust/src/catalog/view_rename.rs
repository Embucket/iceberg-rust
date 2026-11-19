//! Untrusted bounded input for an atomic view namespace/property publication.

use crate::error::Error;

/// Replace exactly one existing opaque property during a checked view rename.
///
/// This is a description, not authorization or a publication capability. The
/// catalog must bind it to the complete expected predecessor under its writer.
/// No insertion, removal, full successor, schema/version edit or callback is
/// accepted. Embedders still validate the meaning of the property and value.
#[derive(Debug)]
pub struct ExistingViewPropertyReplacement {
    key: String,
    expected: String,
    replacement: String,
}

impl ExistingViewPropertyReplacement {
    /// Admit one ASCII property key and two different nonempty UTF-8 values.
    /// Keys are at most 255 bytes; each value is at most 64 KiB and contains no
    /// NUL. SQL parsing, rights and interpretation of opaque markers are outside
    /// this native metadata primitive.
    pub fn try_new(key: String, expected: String, replacement: String) -> Result<Self, Error> {
        let valid_key = !key.is_empty()
            && key.len() <= 255
            && key
                .as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
            && key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
        let valid_value = |value: &str| {
            !value.is_empty() && value.len() <= 65_536 && !value.as_bytes().contains(&0)
        };
        if !valid_key
            || !valid_value(&expected)
            || !valid_value(&replacement)
            || expected == replacement
        {
            return Err(Error::InvalidFormat(
                "view rename requires one bounded existing-property replacement".into(),
            ));
        }
        Ok(Self {
            key,
            expected,
            replacement,
        })
    }

    /// Opaque existing property key; not an object or authorization identifier.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Exact expected old value, additionally bound by full predecessor equality.
    pub fn expected(&self) -> &str {
        &self.expected
    }

    /// Exact replacement value; the catalog never evaluates or rewrites it.
    pub fn replacement(&self) -> &str {
        &self.replacement
    }
}

/// Exactly two existing opaque property replacements in one checked rename.
///
/// The fixed array admits no insertion/removal or caller-supplied successor.
/// Entries retain the single-replacement bounds and are stored in ascending key
/// order. This description is not Clone, serializable, or an authorization token.
#[derive(Debug)]
pub struct ExistingViewPropertyPairReplacement {
    replacements: [ExistingViewPropertyReplacement; 2],
}

impl ExistingViewPropertyPairReplacement {
    /// Consume exactly two bounded descriptions with distinct canonical keys.
    /// Input order is immaterial; duplicate keys are rejected rather than merged.
    pub fn try_new(mut replacements: [ExistingViewPropertyReplacement; 2]) -> Result<Self, Error> {
        if replacements[0].key == replacements[1].key {
            return Err(Error::InvalidFormat(
                "view rename requires exactly two distinct existing-property keys".into(),
            ));
        }
        if replacements[0].key > replacements[1].key {
            replacements.swap(0, 1);
        }
        Ok(Self { replacements })
    }

    /// The two immutable bounded descriptions, in strict ascending key order.
    pub fn replacements(&self) -> &[ExistingViewPropertyReplacement; 2] {
        &self.replacements
    }
}

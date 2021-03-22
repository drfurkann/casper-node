use std::{
    fmt::{self, Debug, Display, Formatter},
    ops::{Add, Sub},
};

use datasize::DataSize;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use casper_types::bytesrepr::{self, FromBytes, ToBytes};

use crate::components::consensus::ConsensusMessage;

#[derive(
    DataSize,
    Debug,
    Clone,
    Copy,
    Hash,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct EraId(pub(crate) u64);

impl EraId {
    pub(crate) fn message(self, payload: Vec<u8>) -> ConsensusMessage {
        ConsensusMessage::Protocol {
            era_id: self,
            payload,
        }
    }

    #[allow(clippy::integer_arithmetic)] // The caller must make sure this doesn't overflow.
    pub(crate) fn successor(self) -> EraId {
        EraId(self.0 + 1)
    }

    /// Returns the current era minus `x`, or `None` if that would be less than `0`.
    pub(crate) fn checked_sub(&self, x: u64) -> Option<EraId> {
        self.0.checked_sub(x).map(EraId)
    }

    /// Returns the current era minus `x`, or `0` if that would be less than `0`.
    pub(crate) fn saturating_sub(&self, x: u64) -> EraId {
        EraId(self.0.saturating_sub(x))
    }

    /// Returns the current era plus `x`, or `u64::MAX` if that would be more than `u64::MAX`.
    pub(crate) fn saturating_add(&self, x: u64) -> EraId {
        EraId(self.0.saturating_add(x))
    }

    /// Returns whether this is era 0.
    pub(crate) fn is_genesis(&self) -> bool {
        self.0 == 0
    }
}

impl Add<u64> for EraId {
    type Output = EraId;

    #[allow(clippy::integer_arithmetic)] // The caller must make sure this doesn't overflow.
    fn add(self, x: u64) -> EraId {
        EraId(self.0 + x)
    }
}

impl Sub<u64> for EraId {
    type Output = EraId;

    #[allow(clippy::integer_arithmetic)] // The caller must make sure this doesn't overflow.
    fn sub(self, x: u64) -> EraId {
        EraId(self.0 - x)
    }
}

impl Display for EraId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "era {}", self.0)
    }
}

impl From<EraId> for u64 {
    fn from(era_id: EraId) -> Self {
        era_id.0
    }
}

impl ToBytes for EraId {
    fn to_bytes(&self) -> Result<Vec<u8>, bytesrepr::Error> {
        self.0.to_bytes()
    }

    fn serialized_length(&self) -> usize {
        self.0.serialized_length()
    }
}

impl FromBytes for EraId {
    fn from_bytes(bytes: &[u8]) -> Result<(Self, &[u8]), bytesrepr::Error> {
        let (id_value, remainder) = u64::from_bytes(bytes)?;
        let era_id = EraId(id_value);
        Ok((era_id, remainder))
    }
}

#[cfg(test)]
mod tests {
    use rand::Rng;

    use super::*;
    use crate::testing::TestRng;

    #[test]
    fn bytesrepr_roundtrip() {
        let mut rng = TestRng::new();
        let era_id = EraId(rng.gen());
        bytesrepr::test_serialization_roundtrip(&era_id);
    }
}

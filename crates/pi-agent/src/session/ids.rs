use std::fmt;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::{Timestamp, Uuid};

macro_rules! id_newtype {
    ($name:ident) => {
        #[doc = "String-backed identifier newtype."]
        #[derive(
            Clone, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
        )]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            #[doc = "Builds a new identifier from a string-like value."]
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }
            #[doc = "Borrows the identifier as a string slice."]
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
            #[doc = "Consumes the newtype and returns the inner string."]
            #[must_use]
            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }
        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }
        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }
        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_newtype!(EntryId);
id_newtype!(OperationId);
id_newtype!(UsageId);
id_newtype!(LaneName);

/// Generate canonical `UUIDv7` identifiers with process-wide ordering state.
///
/// Supplied timestamps are preserved for follower identifiers. Calls without a
/// timestamp clamp the current clock against the last ordinary timestamp.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7Generator;

const MAX_UUID_V7_TIMESTAMP: u64 = 0x0000_ffff_ffff_ffff;
const MAX_SEQUENCE: u64 = (1_u64 << 41) - 1;
const RANDOM_SEQUENCE_MASK: u64 = (1_u64 << 40) - 1;
static GENERATOR_STATE: LazyLock<Mutex<GeneratorState>> =
    LazyLock::new(|| Mutex::new(GeneratorState::default()));

#[derive(Debug, Default)]
struct GeneratorState {
    last_ordinary_timestamp: Option<u64>,
    sequence: Option<u64>,
}

impl GeneratorState {
    fn supplied_timestamp(requested: u64) -> u64 {
        requested
    }

    fn ordinary_timestamp(&mut self, requested: u64) -> u64 {
        let effective = self
            .last_ordinary_timestamp
            .map_or(requested, |last| last.max(requested));
        self.last_ordinary_timestamp = Some(effective);
        effective
    }

    fn next_sequence(
        &mut self,
        random_sequence: impl FnOnce() -> u64,
    ) -> Result<u64, super::error::SessionError> {
        match self.sequence {
            None => {
                let sequence = random_sequence() & RANDOM_SEQUENCE_MASK;
                self.sequence = Some(sequence);
                Ok(sequence)
            }
            Some(sequence) if sequence == MAX_SEQUENCE => {
                Err(super::error::SessionError::Invariant(
                    "UUIDv7 generator sequence exhausted".to_owned(),
                ))
            }
            Some(sequence) => {
                let next = sequence + 1;
                self.sequence = Some(next);
                Ok(next)
            }
        }
    }
}

impl UuidV7Generator {
    /// Construct a generator handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Generate a canonical `UUIDv7` string, preserving an explicit timestamp.
    ///
    /// # Errors
    ///
    /// Returns a [`super::error::SessionError`] when the timestamp is out of
    /// range, when reading the system clock fails, or when the sequence
    /// overflows.
    pub fn next_string(
        &self,
        timestamp_ms: Option<i64>,
    ) -> Result<String, super::error::SessionError> {
        let requested = match timestamp_ms {
            Some(value) => {
                u64::try_from(value).map_err(|_| invalid_timestamp(i128::from(value)))?
            }
            None => current_millis()?,
        };
        validate_timestamp(requested)?;

        let (timestamp, sequence) = {
            let mut state = GENERATOR_STATE.lock().map_err(|_| {
                super::error::SessionError::Invariant(
                    "UUIDv7 generator state lock poisoned".to_owned(),
                )
            })?;
            let timestamp = match timestamp_ms {
                Some(_) => GeneratorState::supplied_timestamp(requested),
                None => state.ordinary_timestamp(requested),
            };
            let sequence = state.next_sequence(random_sequence)?;
            (timestamp, sequence)
        };

        let seconds = timestamp / 1_000;
        let subsec_millis = u32::try_from(timestamp % 1_000).map_err(|_| {
            super::error::SessionError::Invariant("UUIDv7 subsecond timestamp overflow".to_owned())
        })?;
        let subsec_nanos = subsec_millis * 1_000_000;
        let uuid = Uuid::new_v7(Timestamp::from_unix_time(
            seconds,
            subsec_nanos,
            u128::from(sequence),
            41,
        ));
        Ok(uuid.hyphenated().to_string())
    }
}

impl super::traits::IdGenerator for UuidV7Generator {
    fn next(&self, timestamp_ms: Option<i64>) -> Result<String, super::error::SessionError> {
        self.next_string(timestamp_ms)
    }
}

fn current_millis() -> Result<u64, super::error::SessionError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            super::error::SessionError::Backend(super::error::StorageFailure {
                code: super::error::StorageErrorCode::Io,
                message: "failed to read the system clock for UUIDv7".to_owned(),
                source: Some(Arc::new(error)),
            })
        })?;
    u64::try_from(duration.as_millis()).map_err(|_| invalid_timestamp(i128::from(u64::MAX)))
}

fn validate_timestamp(timestamp: u64) -> Result<(), super::error::SessionError> {
    if timestamp > MAX_UUID_V7_TIMESTAMP {
        return Err(invalid_timestamp(i128::from(timestamp)));
    }
    Ok(())
}

fn invalid_timestamp(timestamp: i128) -> super::error::SessionError {
    super::error::SessionError::Invariant(format!(
        "UUIDv7 timestamp must be an integer between 0 and {MAX_UUID_V7_TIMESTAMP}, got {timestamp}",
    ))
}

fn random_sequence() -> u64 {
    let uuid = Uuid::new_v7(Timestamp::from_unix_time(0, 0, 0, 0));
    let bytes = uuid.as_bytes();
    u64::from_be_bytes([
        0, 0, 0, bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::{Variant, Version};

    #[test]
    fn timestamp_boundaries_are_explicit() {
        assert!(validate_timestamp(0).is_ok());
        assert!(validate_timestamp(MAX_UUID_V7_TIMESTAMP).is_ok());
        assert!(validate_timestamp(MAX_UUID_V7_TIMESTAMP + 1).is_err());
    }

    #[test]
    fn invalid_supplied_timestamps_are_rejected() {
        let generator = UuidV7Generator::new();
        assert!(generator.next_string(Some(-1)).is_err());
        assert!(matches!(
            i64::try_from(MAX_UUID_V7_TIMESTAMP + 1),
            Ok(above_max) if generator.next_string(Some(above_max)).is_err()
        ));
    }

    #[test]
    fn supplied_timestamp_does_not_clamp_or_update_ordinary_state() {
        let mut state = GeneratorState {
            last_ordinary_timestamp: Some(100),
            sequence: Some(7),
        };
        assert_eq!(GeneratorState::supplied_timestamp(50), 50);
        assert_eq!(state.last_ordinary_timestamp, Some(100));
        assert_eq!(state.ordinary_timestamp(75), 100);
        assert_eq!(state.last_ordinary_timestamp, Some(100));
    }

    #[test]
    fn sequence_initialization_and_exhaustion_are_bounded() {
        let mut state = GeneratorState::default();
        assert!(matches!(
            state.next_sequence(|| u64::MAX),
            Ok(RANDOM_SEQUENCE_MASK)
        ));
        assert!(
            matches!(state.next_sequence(|| 0), Ok(value) if value == RANDOM_SEQUENCE_MASK + 1)
        );

        state.sequence = Some(MAX_SEQUENCE - 1);
        assert!(matches!(state.next_sequence(|| 0), Ok(MAX_SEQUENCE)));
        assert!(matches!(
            state.next_sequence(|| 0),
            Err(super::super::error::SessionError::Invariant(message)) if message.contains("exhausted")
        ));
    }

    #[test]
    fn generated_large_timestamp_is_canonical_uuid_v7() {
        let generated = UuidV7Generator::new().next_string(Some(65_536));
        assert!(generated.is_ok());
        let Some(id) = generated.ok() else {
            return;
        };
        let parsed = Uuid::parse_str(&id);
        assert!(parsed.is_ok());
        let Some(uuid) = parsed.ok() else {
            return;
        };
        assert_eq!(uuid.get_version(), Some(Version::SortRand));
        assert_eq!(uuid.get_variant(), Variant::RFC4122);
        let Some(timestamp) = uuid.get_timestamp() else {
            return;
        };
        assert_eq!(timestamp.to_unix(), (65, 536_000_000));
    }
}

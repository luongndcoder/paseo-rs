//! Tri-state field helpers that mirror zod's `optional()` / `nullable()` combinations.
//!
//! `undefined` is omitted on write, `null` is kept. `Option<T>` fields use
//! [`strict_optional`] so that a stray `null` is rejected like zod's `.optional()` does.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// `field?: T | null`: missing, explicit null, or a value.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Nullable<T> {
    #[default]
    Missing,
    Null,
    Value(T),
}

impl<T> Nullable<T> {
    pub fn is_missing(&self) -> bool {
        matches!(self, Nullable::Missing)
    }

    pub fn value(&self) -> Option<&T> {
        match self {
            Nullable::Value(v) => Some(v),
            _ => None,
        }
    }

    pub fn into_value(self) -> Option<T> {
        match self {
            Nullable::Value(v) => Some(v),
            _ => None,
        }
    }

    /// `undefined` is the only state that is not "present" in JS terms.
    pub fn is_present(&self) -> bool {
        !self.is_missing()
    }
}

impl<T> From<Option<T>> for Nullable<T> {
    /// `None` becomes an explicit null.
    fn from(value: Option<T>) -> Self {
        match value {
            Some(v) => Nullable::Value(v),
            None => Nullable::Null,
        }
    }
}

impl<T: Serialize> Serialize for Nullable<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            // Callers pair this with `skip_serializing_if = "Nullable::is_missing"`.
            Nullable::Missing | Nullable::Null => serializer.serialize_none(),
            Nullable::Value(v) => v.serialize(serializer),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Nullable<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Option::<T>::deserialize(deserializer)? {
            Some(v) => Nullable::Value(v),
            None => Nullable::Null,
        })
    }
}

/// `z.T().optional()`: missing is fine, `null` is not.
pub fn strict_optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// `z.T().nullable()` without a default: the key must be present, null is fine.
pub fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// Declares a union discriminated by a string field, with an `Other(Value)` fallback for tags
/// (or payloads) this crate does not model. Payload structs do not contain the tag field.
macro_rules! tagged_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident [$tag:literal] {
            $( $(#[$vmeta:meta])* $var:ident($ty:ty) = $wire:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq)]
        $vis enum $name {
            $( $(#[$vmeta])* $var($ty), )+
            /// Unknown tag, or a known tag whose payload did not fit the typed shape.
            Other(::serde_json::Value),
        }

        impl $name {
            pub fn from_value(value: ::serde_json::Value) -> Self {
                let tag = value.get($tag).and_then(|t| t.as_str()).map(str::to_owned);
                match tag.as_deref() {
                    $(
                        Some($wire) => {
                            let mut body = value.clone();
                            if let Some(obj) = body.as_object_mut() {
                                obj.remove($tag);
                            }
                            match ::serde_json::from_value::<$ty>(body) {
                                Ok(payload) => Self::$var(payload),
                                Err(_) => Self::Other(value),
                            }
                        }
                    )+
                    _ => Self::Other(value),
                }
            }

            pub fn to_value(&self) -> ::serde_json::Value {
                match self {
                    $(
                        Self::$var(payload) => {
                            let mut value = ::serde_json::to_value(payload)
                                .unwrap_or(::serde_json::Value::Null);
                            if let Some(obj) = value.as_object_mut() {
                                obj.insert($tag.to_owned(), ::serde_json::Value::from($wire));
                            }
                            value
                        }
                    )+
                    Self::Other(value) => value.clone(),
                }
            }

            /// The wire tag, if the frame carries one.
            pub fn tag(&self) -> Option<&str> {
                match self {
                    $( Self::$var(_) => Some($wire), )+
                    Self::Other(value) => value.get($tag).and_then(|t| t.as_str()),
                }
            }

            pub fn is_other(&self) -> bool {
                matches!(self, Self::Other(_))
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.to_value().serialize(serializer)
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = <::serde_json::Value as ::serde::Deserialize>::deserialize(deserializer)?;
                Ok(Self::from_value(value))
            }
        }
    };
}

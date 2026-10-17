use std::fmt;
use std::marker::PhantomData;

use serde::de::{MapAccess, Visitor, value::MapAccessDeserializer};
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

pub(crate) const MAX_PRIVATE_REQUEST_BYTES: usize = 32_768;

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BindingGeneration {
    #[serde(deserialize_with = "canonical_uuid")]
    pub(crate) grant_id: Uuid,
    pub(crate) grant_revision: u64,
    #[serde(deserialize_with = "canonical_uuid")]
    pub(crate) host_epoch: Uuid,
    #[serde(deserialize_with = "canonical_uuid")]
    pub(crate) connection_epoch: Uuid,
}

pub(super) fn canonical_uuid<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Uuid, D::Error> {
    let encoded = String::deserialize(deserializer)?;
    let uuid = Uuid::parse_str(&encoded)
        .map_err(|_| serde::de::Error::custom("invalid Settings identifier"))?;
    if uuid.is_nil() || encoded.len() != 36 || encoded != uuid.to_string() {
        return Err(serde::de::Error::custom("invalid Settings identifier"));
    }
    Ok(uuid)
}

pub(super) fn object<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<T, D::Error> {
    Object::deserialize(deserializer).map(|Object(value)| value)
}

struct Object<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for ObjectVisitor<T> {
            type Value = Object<T>;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a Settings object")
            }
            fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
                T::deserialize(MapAccessDeserializer::new(map)).map(Object)
            }
        }
        deserializer.deserialize_map(ObjectVisitor(PhantomData))
    }
}

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::VaultError;

const SCHEME: &str = "vault://";
pub const MAX_REFERENCE_BYTES: usize = 512;
const MAX_SEGMENT_BYTES: usize = 128;
const MAX_FIELD_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum VaultProviderId {
    #[serde(rename = "local")]
    Local,
    #[serde(rename = "1password")]
    OnePassword,
    #[serde(rename = "protonpass")]
    ProtonPass,
}

impl VaultProviderId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::OnePassword => "1password",
            Self::ProtonPass => "protonpass",
        }
    }

    fn segments(self) -> usize {
        match self {
            Self::Local => 1,
            Self::OnePassword => 3,
            Self::ProtonPass => 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct VaultReference {
    provider: VaultProviderId,
    id: String,
    field: Option<String>,
}

impl VaultReference {
    pub fn local(id: uuid::Uuid, field: Option<&str>) -> Result<Self, VaultError> {
        let reference = Self {
            provider: VaultProviderId::Local,
            id: id.hyphenated().to_string(),
            field: field.map(str::to_owned),
        };
        reference.to_string().parse()
    }

    pub fn provider(&self) -> VaultProviderId {
        self.provider
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn field(&self) -> Option<&str> {
        self.field.as_deref()
    }

    pub fn local_id(&self) -> Result<uuid::Uuid, VaultError> {
        if self.provider != VaultProviderId::Local {
            return Err(VaultError::InvalidReference);
        }
        uuid::Uuid::parse_str(&self.id).map_err(|_| VaultError::InvalidReference)
    }
}

impl fmt::Display for VaultReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{SCHEME}{}/{}", self.provider.as_str(), self.id)?;
        if let Some(field) = &self.field {
            write!(formatter, "#{field}")?;
        }
        Ok(())
    }
}

impl FromStr for VaultReference {
    type Err = VaultError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() > MAX_REFERENCE_BYTES {
            return Err(VaultError::InvalidReference);
        }
        let rest = value
            .strip_prefix(SCHEME)
            .ok_or(VaultError::InvalidReference)?;
        let (path, field) = match rest.split_once('#') {
            Some((path, field)) => (path, Some(field)),
            None => (rest, None),
        };
        if field.is_some_and(|field| !valid_field(field)) {
            return Err(VaultError::InvalidReference);
        }
        let (provider, id) = path.split_once('/').ok_or(VaultError::InvalidReference)?;
        let provider = match provider {
            "local" => VaultProviderId::Local,
            "1password" => VaultProviderId::OnePassword,
            "protonpass" => VaultProviderId::ProtonPass,
            _ => return Err(VaultError::InvalidReference),
        };
        let segments: Vec<&str> = id.split('/').collect();
        if segments.len() != provider.segments() {
            return Err(VaultError::InvalidReference);
        }
        let valid = match provider {
            VaultProviderId::Local => uuid::Uuid::parse_str(id)
                .is_ok_and(|uuid| !uuid.is_nil() && uuid.hyphenated().to_string() == id),
            _ => segments.iter().all(|segment| valid_segment(segment)),
        };
        if !valid {
            return Err(VaultError::InvalidReference);
        }
        Ok(Self {
            provider,
            id: id.to_owned(),
            field: field.map(str::to_owned),
        })
    }
}

impl Serialize for VaultReference {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for VaultReference {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

fn valid_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SEGMENT_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_=".contains(&byte))
}

fn valid_field(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_FIELD_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}

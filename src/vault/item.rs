use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::host::credentials::SecretTransfer;
use crate::protocol::UnixMillis;

use super::{VaultError, VaultProviderId, VaultReference};

const MAX_LABEL_CHARS: usize = 128;
const MAX_TEXT_CHARS: usize = 256;
const MAX_SITES: usize = 16;
const MAX_HOST_BYTES: usize = 253;
const MAX_ENV_NAME_BYTES: usize = 128;
pub const MAX_METADATA_BYTES: usize = 4_096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultItemKind {
    Login,
    ApiSecret,
    Card,
    Identity,
}

impl VaultItemKind {
    pub fn secret_fields(self) -> &'static [&'static str] {
        match self {
            Self::Login => &["password", "totp"],
            Self::ApiSecret => &["value"],
            Self::Card => &["number", "cvc", "holder"],
            Self::Identity => &["email", "phone", "address"],
        }
    }

    fn required_field(self) -> Option<&'static str> {
        match self {
            Self::Login => Some("password"),
            Self::ApiSecret => Some("value"),
            Self::Card => Some("number"),
            Self::Identity => None,
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        [Self::Login, Self::ApiSecret, Self::Card, Self::Identity]
            .into_iter()
            .find(|kind| kind.as_str() == value)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::ApiSecret => "api_secret",
            Self::Card => "card",
            Self::Identity => "identity",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum VaultItemMetadata {
    Login {
        label: String,
        #[serde(default)]
        sites: Vec<String>,
        #[serde(default)]
        username: Option<String>,
    },
    ApiSecret {
        label: String,
        #[serde(default)]
        env_name: Option<String>,
        #[serde(default)]
        service: Option<String>,
    },
    Card {
        label: String,
        #[serde(default)]
        brand: Option<String>,
        #[serde(default)]
        last4: Option<String>,
        #[serde(default)]
        expiry_month: Option<u8>,
        #[serde(default)]
        expiry_year: Option<u16>,
    },
    Identity {
        label: String,
    },
}

impl VaultItemMetadata {
    pub fn kind(&self) -> VaultItemKind {
        match self {
            Self::Login { .. } => VaultItemKind::Login,
            Self::ApiSecret { .. } => VaultItemKind::ApiSecret,
            Self::Card { .. } => VaultItemKind::Card,
            Self::Identity { .. } => VaultItemKind::Identity,
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Login { label, .. }
            | Self::ApiSecret { label, .. }
            | Self::Card { label, .. }
            | Self::Identity { label } => label,
        }
    }

    pub fn validate(&self) -> Result<(), VaultError> {
        text(self.label(), MAX_LABEL_CHARS, "label")?;
        match self {
            Self::Login {
                sites, username, ..
            } => {
                if sites.len() > MAX_SITES {
                    return Err(VaultError::InvalidItem("at most sixteen sites are allowed"));
                }
                if !sites.iter().all(|site| valid_host(site)) {
                    return Err(VaultError::InvalidItem(
                        "sites must be lowercase host names without scheme, port or path",
                    ));
                }
                optional_text(username.as_deref(), MAX_TEXT_CHARS, "username")
            }
            Self::ApiSecret {
                env_name, service, ..
            } => {
                if env_name
                    .as_deref()
                    .is_some_and(|name| !valid_env_name(name))
                {
                    return Err(VaultError::InvalidItem(
                        "env_name must be an uppercase environment variable name",
                    ));
                }
                optional_text(service.as_deref(), MAX_LABEL_CHARS, "service")
            }
            Self::Card {
                brand,
                last4,
                expiry_month,
                expiry_year,
                ..
            } => {
                optional_text(brand.as_deref(), 32, "brand")?;
                if last4.as_deref().is_some_and(|last4| {
                    last4.len() != 4 || !last4.bytes().all(|byte| byte.is_ascii_digit())
                }) {
                    return Err(VaultError::InvalidItem("last4 must be exactly four digits"));
                }
                if expiry_month.is_some_and(|month| !(1..=12).contains(&month))
                    || expiry_year.is_some_and(|year| !(2000..=2100).contains(&year))
                {
                    return Err(VaultError::InvalidItem("invalid card expiry"));
                }
                Ok(())
            }
            Self::Identity { .. } => Ok(()),
        }
    }

    fn sites(&self) -> Vec<String> {
        match self {
            Self::Login { sites, .. } => sites.clone(),
            _ => Vec::new(),
        }
    }

    fn username(&self) -> Option<String> {
        match self {
            Self::Login { username, .. } => username.clone(),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultPutRequest {
    pub metadata: VaultItemMetadata,
    pub secrets: BTreeMap<String, SecretTransfer>,
}

impl VaultPutRequest {
    pub fn validate(&self) -> Result<(), VaultError> {
        self.metadata.validate()?;
        let kind = self.metadata.kind();
        if self.secrets.is_empty() {
            return Err(VaultError::InvalidItem(
                "at least one secret field is required",
            ));
        }
        if !self
            .secrets
            .keys()
            .all(|field| kind.secret_fields().contains(&field.as_str()))
        {
            return Err(VaultError::InvalidItem(
                "secret field is not defined for this item kind",
            ));
        }
        if kind
            .required_field()
            .is_some_and(|field| !self.secrets.contains_key(field))
        {
            return Err(VaultError::InvalidItem(
                "a required secret field for this item kind is missing",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultItemSummary {
    #[serde(rename = "ref")]
    pub reference: VaultReference,
    pub provider: VaultProviderId,
    pub kind: VaultItemKind,
    pub label: String,
    pub sites: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub fields: Vec<String>,
    pub metadata: VaultItemMetadata,
    pub updated_at: UnixMillis,
}

impl VaultItemSummary {
    pub(crate) fn new(
        reference: VaultReference,
        metadata: VaultItemMetadata,
        fields: Vec<String>,
        updated_at: UnixMillis,
    ) -> Self {
        Self {
            provider: reference.provider(),
            reference,
            kind: metadata.kind(),
            label: metadata.label().to_owned(),
            sites: metadata.sites(),
            username: metadata.username(),
            fields,
            metadata,
            updated_at,
        }
    }

    pub(crate) fn matches(&self, query: &str, kinds: &[VaultItemKind]) -> bool {
        if !kinds.is_empty() && !kinds.contains(&self.kind) {
            return false;
        }
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        let contains = |value: &str| value.to_lowercase().contains(&query);
        contains(&self.label)
            || self.sites.iter().any(|site| contains(site))
            || self.username.as_deref().is_some_and(contains)
            || match &self.metadata {
                VaultItemMetadata::ApiSecret {
                    env_name, service, ..
                } => {
                    env_name.as_deref().is_some_and(contains)
                        || service.as_deref().is_some_and(contains)
                }
                VaultItemMetadata::Card { brand, .. } => brand.as_deref().is_some_and(contains),
                _ => false,
            }
    }
}

fn text(value: &str, limit: usize, field: &'static str) -> Result<(), VaultError> {
    if value.trim().is_empty()
        || value.chars().count() > limit
        || value.chars().any(char::is_control)
    {
        return Err(VaultError::InvalidMetadata(field));
    }
    Ok(())
}

fn optional_text(value: Option<&str>, limit: usize, field: &'static str) -> Result<(), VaultError> {
    value.map_or(Ok(()), |value| text(value, limit, field))
}

pub(crate) fn valid_host(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_HOST_BYTES
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

pub(crate) fn valid_env_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    value.len() <= MAX_ENV_NAME_BYTES
        && bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_uppercase() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

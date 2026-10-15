use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

const MAX_DIMENSIONS: usize = 65_536;
const MAX_INPUT_BYTES: usize = 1024 * 1024;
const MAX_TITLE_BYTES: usize = 4_096;
const MAX_BATCH_INPUTS: usize = 64;
const MAX_BATCH_BYTES: usize = 16 * 1024 * 1024;
const UNIT_NORM_TOLERANCE: f64 = 0.0001;

#[derive(Debug, Error)]
pub enum EmbeddingError {
    #[error("invalid embedding {field}: {reason}")]
    Invalid {
        field: &'static str,
        reason: &'static str,
    },
    #[error("embedding {resource} exceeds limit {limit}")]
    Limit {
        resource: &'static str,
        limit: usize,
    },
    #[error("unsupported embedding dimension {0}")]
    UnsupportedDimension(usize),
    #[error("embedding vector dimension is {actual}; expected {expected}")]
    VectorDimension { expected: usize, actual: usize },
    #[error("embedding vector contains a nonfinite value at dimension {0}")]
    NonfiniteVector(usize),
    #[error("embedding vector has zero norm")]
    ZeroNorm,
    #[error("embedding vector must have unit L2 norm; actual norm is {0}")]
    NotNormalized(f64),
    #[error("incompatible embedding profile: expected {expected}, found {actual}")]
    IncompatibleProfile {
        expected: EmbeddingFingerprint,
        actual: EmbeddingFingerprint,
    },
    #[error("embedding operation cancelled")]
    Cancelled,
    #[error("embedding backend failed: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingPurpose {
    Query,
    Document,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "EmbeddingInputData")]
pub struct EmbeddingInput {
    purpose: EmbeddingPurpose,
    text: String,
    title: Option<String>,
}

impl EmbeddingInput {
    pub fn query(text: impl Into<String>) -> Result<Self, EmbeddingError> {
        Self::new(EmbeddingPurpose::Query, text.into(), None)
    }

    pub fn document(
        text: impl Into<String>,
        title: Option<String>,
    ) -> Result<Self, EmbeddingError> {
        Self::new(EmbeddingPurpose::Document, text.into(), title)
    }

    pub fn purpose(&self) -> EmbeddingPurpose {
        self.purpose
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    fn new(
        purpose: EmbeddingPurpose,
        text: String,
        title: Option<String>,
    ) -> Result<Self, EmbeddingError> {
        if text.trim().is_empty() {
            return Err(invalid("input text", "must not be blank"));
        }
        if text.len() > MAX_INPUT_BYTES {
            return Err(EmbeddingError::Limit {
                resource: "input bytes",
                limit: MAX_INPUT_BYTES,
            });
        }
        if title
            .as_ref()
            .is_some_and(|value| value.len() > MAX_TITLE_BYTES)
        {
            return Err(EmbeddingError::Limit {
                resource: "title bytes",
                limit: MAX_TITLE_BYTES,
            });
        }
        if purpose == EmbeddingPurpose::Query && title.is_some() {
            return Err(invalid(
                "query title",
                "queries cannot have document titles",
            ));
        }
        Ok(Self {
            purpose,
            text,
            title,
        })
    }

    fn byte_len(&self) -> usize {
        self.text.len() + self.title.as_ref().map_or(0, String::len)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmbeddingInputData {
    purpose: EmbeddingPurpose,
    text: String,
    title: Option<String>,
}

impl TryFrom<EmbeddingInputData> for EmbeddingInput {
    type Error = EmbeddingError;

    fn try_from(value: EmbeddingInputData) -> Result<Self, Self::Error> {
        Self::new(value.purpose, value.text, value.title)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingNormalization {
    None,
    UnitL2,
}

impl EmbeddingNormalization {
    fn fingerprint_key(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::UnitL2 => "unit_l2",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingMetric {
    Cosine,
    DotProduct,
    SquaredEuclidean,
}

impl EmbeddingMetric {
    fn fingerprint_key(self) -> &'static str {
        match self {
            Self::Cosine => "cosine",
            Self::DotProduct => "dot_product",
            Self::SquaredEuclidean => "squared_euclidean",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "EmbeddingPreprocessingData")]
pub struct EmbeddingPreprocessing {
    revision: String,
    query_template: String,
    document_template: String,
    missing_title: String,
}

impl EmbeddingPreprocessing {
    pub fn new(
        revision: impl Into<String>,
        query_template: impl Into<String>,
        document_template: impl Into<String>,
        missing_title: impl Into<String>,
    ) -> Result<Self, EmbeddingError> {
        let value = Self {
            revision: revision.into(),
            query_template: query_template.into(),
            document_template: document_template.into(),
            missing_title: missing_title.into(),
        };
        validate_identifier("preprocessing revision", &value.revision)?;
        validate_template(&value.query_template, EmbeddingPurpose::Query)?;
        validate_template(&value.document_template, EmbeddingPurpose::Document)?;
        if value.missing_title.len() > MAX_TITLE_BYTES {
            return Err(EmbeddingError::Limit {
                resource: "missing-title bytes",
                limit: MAX_TITLE_BYTES,
            });
        }
        Ok(value)
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }

    pub fn query_template(&self) -> &str {
        &self.query_template
    }

    pub fn document_template(&self) -> &str {
        &self.document_template
    }

    pub fn missing_title(&self) -> &str {
        &self.missing_title
    }

    pub fn format(&self, input: &EmbeddingInput) -> String {
        let mut template = match input.purpose {
            EmbeddingPurpose::Query => self.query_template.as_str(),
            EmbeddingPurpose::Document => self.document_template.as_str(),
        };
        let title = input
            .title()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or(&self.missing_title);
        let mut formatted = String::with_capacity(template.len() + input.byte_len() + title.len());
        while let Some(start) = template.find('{') {
            formatted.push_str(&template[..start]);
            if template[start..].starts_with("{text}") {
                formatted.push_str(input.text());
                template = &template[start + "{text}".len()..];
            } else {
                formatted.push_str(title);
                template = &template[start + "{title}".len()..];
            }
        }
        formatted.push_str(template);
        formatted
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmbeddingPreprocessingData {
    revision: String,
    query_template: String,
    document_template: String,
    missing_title: String,
}

impl TryFrom<EmbeddingPreprocessingData> for EmbeddingPreprocessing {
    type Error = EmbeddingError;

    fn try_from(value: EmbeddingPreprocessingData) -> Result<Self, Self::Error> {
        Self::new(
            value.revision,
            value.query_template,
            value.document_template,
            value.missing_title,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "EmbeddingSpecData")]
pub struct EmbeddingSpec {
    model: String,
    revision: String,
    supported_dimensions: Vec<usize>,
    preprocessing: EmbeddingPreprocessing,
    normalization: EmbeddingNormalization,
    metric: EmbeddingMetric,
}

impl EmbeddingSpec {
    pub fn new(
        model: impl Into<String>,
        revision: impl Into<String>,
        mut supported_dimensions: Vec<usize>,
        preprocessing: EmbeddingPreprocessing,
        normalization: EmbeddingNormalization,
        metric: EmbeddingMetric,
    ) -> Result<Self, EmbeddingError> {
        let model = model.into();
        let revision = revision.into();
        validate_identifier("model identifier", &model)?;
        validate_identifier("model revision", &revision)?;
        if supported_dimensions.is_empty() || supported_dimensions.len() > 64 {
            return Err(invalid(
                "supported dimensions",
                "must contain between 1 and 64 values",
            ));
        }
        supported_dimensions.sort_unstable();
        if supported_dimensions
            .iter()
            .any(|dimension| *dimension == 0 || *dimension > MAX_DIMENSIONS)
            || supported_dimensions
                .windows(2)
                .any(|pair| pair[0] == pair[1])
        {
            return Err(invalid(
                "supported dimensions",
                "must be unique values between 1 and 65536",
            ));
        }
        Ok(Self {
            model,
            revision,
            supported_dimensions,
            preprocessing,
            normalization,
            metric,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }

    pub fn supported_dimensions(&self) -> &[usize] {
        &self.supported_dimensions
    }

    pub fn preprocessing(&self) -> &EmbeddingPreprocessing {
        &self.preprocessing
    }

    pub fn normalization(&self) -> EmbeddingNormalization {
        self.normalization
    }

    pub fn metric(&self) -> EmbeddingMetric {
        self.metric
    }

    pub fn fingerprint(&self) -> EmbeddingFingerprint {
        let mut hash = Sha256::new();
        hash.update(b"everything.embedding.spec.v1\0");
        hash_string(&mut hash, &self.model);
        hash_string(&mut hash, &self.revision);
        hash.update((self.supported_dimensions.len() as u64).to_le_bytes());
        for dimension in &self.supported_dimensions {
            hash.update((*dimension as u32).to_le_bytes());
        }
        hash_string(&mut hash, &self.preprocessing.revision);
        hash_string(&mut hash, &self.preprocessing.query_template);
        hash_string(&mut hash, &self.preprocessing.document_template);
        hash_string(&mut hash, &self.preprocessing.missing_title);
        hash_string(&mut hash, self.normalization.fingerprint_key());
        hash_string(&mut hash, self.metric.fingerprint_key());
        finish_fingerprint(hash)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmbeddingSpecData {
    model: String,
    revision: String,
    supported_dimensions: Vec<usize>,
    preprocessing: EmbeddingPreprocessing,
    normalization: EmbeddingNormalization,
    metric: EmbeddingMetric,
}

impl TryFrom<EmbeddingSpecData> for EmbeddingSpec {
    type Error = EmbeddingError;

    fn try_from(value: EmbeddingSpecData) -> Result<Self, Self::Error> {
        Self::new(
            value.model,
            value.revision,
            value.supported_dimensions,
            value.preprocessing,
            value.normalization,
            value.metric,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "EmbeddingProfileData")]
pub struct EmbeddingProfile {
    spec: EmbeddingSpec,
    dimensions: usize,
}

impl EmbeddingProfile {
    pub fn new(spec: EmbeddingSpec, dimensions: usize) -> Result<Self, EmbeddingError> {
        if !spec.supported_dimensions.contains(&dimensions) {
            return Err(EmbeddingError::UnsupportedDimension(dimensions));
        }
        Ok(Self { spec, dimensions })
    }

    pub fn spec(&self) -> &EmbeddingSpec {
        &self.spec
    }

    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    pub fn fingerprint(&self) -> EmbeddingFingerprint {
        let mut hash = Sha256::new();
        hash.update(b"everything.embedding.profile.v1\0");
        hash_string(&mut hash, self.spec.fingerprint().as_str());
        hash.update((self.dimensions as u32).to_le_bytes());
        finish_fingerprint(hash)
    }

    pub fn ensure_compatible(
        &self,
        fingerprint: &EmbeddingFingerprint,
    ) -> Result<(), EmbeddingError> {
        ensure_fingerprint(self.fingerprint(), fingerprint)
    }

    pub fn validate_vector(&self, values: &[f32]) -> Result<(), EmbeddingError> {
        if values.len() != self.dimensions {
            return Err(EmbeddingError::VectorDimension {
                expected: self.dimensions,
                actual: values.len(),
            });
        }
        let norm = l2_norm(values)?;
        if self.spec.normalization == EmbeddingNormalization::UnitL2 {
            if norm == 0.0 {
                return Err(EmbeddingError::ZeroNorm);
            }
            if (norm - 1.0).abs() > UNIT_NORM_TOLERANCE {
                return Err(EmbeddingError::NotNormalized(norm));
            }
        } else if self.spec.metric == EmbeddingMetric::Cosine && norm == 0.0 {
            return Err(EmbeddingError::ZeroNorm);
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmbeddingProfileData {
    spec: EmbeddingSpec,
    dimensions: usize,
}

impl TryFrom<EmbeddingProfileData> for EmbeddingProfile {
    type Error = EmbeddingError;

    fn try_from(value: EmbeddingProfileData) -> Result<Self, Self::Error> {
        Self::new(value.spec, value.dimensions)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "EmbeddingIndexSpecData")]
pub struct EmbeddingIndexSpec {
    profile: EmbeddingProfile,
    document_schema: String,
}

impl EmbeddingIndexSpec {
    pub fn new(
        profile: EmbeddingProfile,
        document_schema: impl Into<String>,
    ) -> Result<Self, EmbeddingError> {
        let document_schema = document_schema.into();
        validate_identifier("document schema", &document_schema)?;
        Ok(Self {
            profile,
            document_schema,
        })
    }

    pub fn profile(&self) -> &EmbeddingProfile {
        &self.profile
    }

    pub fn document_schema(&self) -> &str {
        &self.document_schema
    }

    pub fn fingerprint(&self) -> EmbeddingFingerprint {
        let mut hash = Sha256::new();
        hash.update(b"everything.embedding.index.v1\0");
        hash_string(&mut hash, self.profile.fingerprint().as_str());
        hash_string(&mut hash, &self.document_schema);
        finish_fingerprint(hash)
    }

    pub fn ensure_compatible(
        &self,
        fingerprint: &EmbeddingFingerprint,
    ) -> Result<(), EmbeddingError> {
        ensure_fingerprint(self.fingerprint(), fingerprint)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmbeddingIndexSpecData {
    profile: EmbeddingProfile,
    document_schema: String,
}

impl TryFrom<EmbeddingIndexSpecData> for EmbeddingIndexSpec {
    type Error = EmbeddingError;

    fn try_from(value: EmbeddingIndexSpecData) -> Result<Self, Self::Error> {
        Self::new(value.profile, value.document_schema)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EmbeddingFingerprint(String);

impl EmbeddingFingerprint {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EmbeddingFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for EmbeddingFingerprint {
    type Err = EmbeddingError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::try_from(value.to_owned())
    }
}

impl TryFrom<String> for EmbeddingFingerprint {
    type Error = EmbeddingError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid(
                "fingerprint",
                "must be a SHA-256 hexadecimal digest",
            ));
        }
        Ok(Self(value.to_ascii_lowercase()))
    }
}

impl From<EmbeddingFingerprint> for String {
    fn from(value: EmbeddingFingerprint) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EmbeddingVector {
    profile_fingerprint: EmbeddingFingerprint,
    values: Vec<f32>,
}

impl EmbeddingVector {
    pub fn new(profile: &EmbeddingProfile, values: Vec<f32>) -> Result<Self, EmbeddingError> {
        profile.validate_vector(&values)?;
        Ok(Self {
            profile_fingerprint: profile.fingerprint(),
            values,
        })
    }

    pub fn profile_fingerprint(&self) -> &EmbeddingFingerprint {
        &self.profile_fingerprint
    }

    pub fn values(&self) -> &[f32] {
        &self.values
    }

    pub fn into_values(self) -> Vec<f32> {
        self.values
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum EmbeddingConfig {
    #[default]
    Disabled,
    Enabled {
        profile: EmbeddingProfile,
    },
}

impl EmbeddingConfig {
    pub fn profile(&self) -> Option<&EmbeddingProfile> {
        match self {
            Self::Disabled => None,
            Self::Enabled { profile } => Some(profile),
        }
    }
}

pub trait Embedder: Send + Sync {
    fn profile(&self) -> &EmbeddingProfile;

    fn embed(
        &self,
        input: &EmbeddingInput,
        cancellation: &CancellationToken,
    ) -> Result<EmbeddingVector, EmbeddingError>;
}

pub fn embed_batch(
    embedder: &dyn Embedder,
    inputs: &[EmbeddingInput],
    cancellation: &CancellationToken,
) -> Result<Vec<EmbeddingVector>, EmbeddingError> {
    if cancellation.is_cancelled() {
        return Err(EmbeddingError::Cancelled);
    }
    if inputs.len() > MAX_BATCH_INPUTS {
        return Err(EmbeddingError::Limit {
            resource: "batch input count",
            limit: MAX_BATCH_INPUTS,
        });
    }
    let input_bytes: usize = inputs.iter().map(EmbeddingInput::byte_len).sum();
    if input_bytes > MAX_BATCH_BYTES {
        return Err(EmbeddingError::Limit {
            resource: "batch input bytes",
            limit: MAX_BATCH_BYTES,
        });
    }
    let mut vectors = Vec::with_capacity(inputs.len());
    let profile = embedder.profile();
    for input in inputs {
        if cancellation.is_cancelled() {
            return Err(EmbeddingError::Cancelled);
        }
        let vector = embedder.embed(input, cancellation)?;
        if cancellation.is_cancelled() {
            return Err(EmbeddingError::Cancelled);
        }
        profile.ensure_compatible(vector.profile_fingerprint())?;
        profile.validate_vector(vector.values())?;
        vectors.push(vector);
    }
    Ok(vectors)
}

pub fn normalize_l2(values: &mut [f32]) -> Result<(), EmbeddingError> {
    if values.len() > MAX_DIMENSIONS {
        return Err(EmbeddingError::Limit {
            resource: "vector dimensions",
            limit: MAX_DIMENSIONS,
        });
    }
    let norm = l2_norm(values)?;
    if norm == 0.0 {
        return Err(EmbeddingError::ZeroNorm);
    }
    for value in values {
        *value = (f64::from(*value) / norm) as f32;
    }
    Ok(())
}

fn l2_norm(values: &[f32]) -> Result<f64, EmbeddingError> {
    let mut squared_norm = 0.0_f64;
    for (index, value) in values.iter().enumerate() {
        if !value.is_finite() {
            return Err(EmbeddingError::NonfiniteVector(index));
        }
        squared_norm += f64::from(*value).powi(2);
    }
    Ok(squared_norm.sqrt())
}

fn validate_identifier(field: &'static str, value: &str) -> Result<(), EmbeddingError> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(invalid(
            field,
            "must contain 1 to 256 bytes without surrounding whitespace or controls",
        ));
    }
    Ok(())
}

fn validate_template(template: &str, purpose: EmbeddingPurpose) -> Result<(), EmbeddingError> {
    if template.len() > 4_096 {
        return Err(EmbeddingError::Limit {
            resource: "preprocessing template bytes",
            limit: 4_096,
        });
    }
    let mut remaining = template;
    let mut text_count = 0;
    let mut title_count = 0;
    while let Some(start) = remaining.find(['{', '}']) {
        if remaining[start..].starts_with("{text}") {
            text_count += 1;
            remaining = &remaining[start + "{text}".len()..];
        } else if remaining[start..].starts_with("{title}") {
            title_count += 1;
            remaining = &remaining[start + "{title}".len()..];
        } else {
            return Err(invalid(
                "preprocessing template",
                "only {text} and {title} placeholders are supported",
            ));
        }
    }
    if text_count != 1
        || title_count > 1
        || (purpose == EmbeddingPurpose::Query && title_count != 0)
    {
        return Err(invalid(
            "preprocessing template",
            "requires one {text} and at most one document {title}",
        ));
    }
    Ok(())
}

fn hash_string(hash: &mut Sha256, value: &str) {
    hash.update((value.len() as u64).to_le_bytes());
    hash.update(value.as_bytes());
}

fn finish_fingerprint(hash: Sha256) -> EmbeddingFingerprint {
    EmbeddingFingerprint(format!("{:x}", hash.finalize()))
}

fn ensure_fingerprint(
    expected: EmbeddingFingerprint,
    actual: &EmbeddingFingerprint,
) -> Result<(), EmbeddingError> {
    if expected != *actual {
        return Err(EmbeddingError::IncompatibleProfile {
            expected,
            actual: actual.clone(),
        });
    }
    Ok(())
}

fn invalid(field: &'static str, reason: &'static str) -> EmbeddingError {
    EmbeddingError::Invalid { field, reason }
}

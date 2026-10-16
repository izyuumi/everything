use std::ffi::{c_char, c_int};
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, ffi, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::embedding::{
    EmbeddingError, EmbeddingIndexSpec, EmbeddingMetric, EmbeddingNormalization, EmbeddingProfile,
    EmbeddingVector,
};

const APPLICATION_ID: i32 = 0x4556_4543;
const SCHEMA: i32 = 1;

#[derive(Debug, Error)]
pub enum VectorError {
    #[error("invalid vector index configuration: {0}")]
    Invalid(&'static str),
    #[error("vector index {0} exceeds its resource limit")]
    Limit(&'static str),
    #[error("vector index is incompatible or malformed")]
    Incompatible,
    #[error("vector index database failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("vector index metadata failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("vector index embedding failed: {0}")]
    Embedding(#[from] EmbeddingError),
    #[error("vector index wiki source failed: {0}")]
    Wiki(#[from] super::WikiError),
    #[error("vector index operation cancelled")]
    Cancelled,
    #[error("wiki source changed during vector generation")]
    SourceChanged,
}

#[derive(Clone, Debug)]
pub struct VectorLimits {
    pub max_vectors: usize,
    pub max_database_bytes: usize,
}

impl Default for VectorLimits {
    fn default() -> Self {
        Self {
            max_vectors: 50_000,
            max_database_bytes: 512 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenerationStatus {
    pub id: i64,
    pub spec: EmbeddingIndexSpec,
    pub source_fingerprint: String,
    pub ready: bool,
    pub after_chunk_id: Option<String>,
    pub vectors: usize,
    pub embedded: usize,
    pub reused: usize,
    pub skipped_blank: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorStatus {
    pub active: Option<GenerationStatus>,
    pub building: Option<GenerationStatus>,
    pub last_error: Option<String>,
}

pub struct VectorIndex {
    connection: Connection,
    limits: VectorLimits,
}

impl VectorIndex {
    pub fn open(path: impl AsRef<Path>, limits: VectorLimits) -> Result<Self, VectorError> {
        if !(1..=100_000).contains(&limits.max_vectors)
            || !(16 * 1024 * 1024..=2 * 1024 * 1024 * 1024).contains(&limits.max_database_bytes)
        {
            return Err(VectorError::Invalid("vector count or database byte bound"));
        }
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(2))?;
        let application: i32 =
            connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
        let version: i32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let tables: i64 = connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        if !((application == 0 && version == 0 && tables == 0)
            || (application == APPLICATION_ID && version == SCHEMA))
        {
            return Err(VectorError::Incompatible);
        }
        initialize_extension(&mut connection)?;
        let extension: String =
            connection.query_row("SELECT vec_version()", [], |row| row.get(0))?;
        if extension != "v0.1.9" {
            return Err(VectorError::Incompatible);
        }
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
        connection.pragma_update(None, "journal_size_limit", 4 * 1024 * 1024)?;
        let page_size: usize =
            connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
        if page_size == 0 {
            return Err(VectorError::Incompatible);
        }
        connection.pragma_update(
            None,
            "max_page_count",
            limits.max_database_bytes / page_size,
        )?;
        let actual_pages: usize =
            connection.pragma_query_value(None, "max_page_count", |row| row.get(0))?;
        if actual_pages > limits.max_database_bytes / page_size {
            return Err(VectorError::Limit("existing database pages"));
        }
        if application == 0 {
            let transaction = connection.transaction()?;
            transaction.execute_batch(
                "CREATE TABLE vector_generations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                fingerprint TEXT NOT NULL CHECK(length(fingerprint)=64),
                spec TEXT NOT NULL CHECK(length(CAST(spec AS BLOB))<=32768),
                source_fingerprint TEXT NOT NULL CHECK(length(source_fingerprint)=64),
                ready INTEGER NOT NULL CHECK(ready IN (0,1)),
                after_chunk_id TEXT,
                vectors INTEGER NOT NULL CHECK(vectors>=0),
                embedded INTEGER NOT NULL CHECK(embedded>=0),
                reused INTEGER NOT NULL CHECK(reused>=0),
                skipped_blank INTEGER NOT NULL CHECK(skipped_blank>=0)
            ) STRICT;
            CREATE TABLE vector_state (
                singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                active_generation INTEGER REFERENCES vector_generations(id),
                last_error TEXT CHECK(last_error IS NULL OR length(CAST(last_error AS BLOB))<=1024)
            ) STRICT;
            INSERT INTO vector_state(singleton) VALUES(1);
            CREATE TABLE vector_inputs (
                generation INTEGER NOT NULL REFERENCES vector_generations(id) ON DELETE CASCADE,
                chunk_id TEXT NOT NULL CHECK(length(chunk_id)=64),
                input_hash TEXT NOT NULL CHECK(length(input_hash)=64),
                PRIMARY KEY(generation,chunk_id)
            ) STRICT;",
            )?;
            transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
            transaction.pragma_update(None, "user_version", SCHEMA)?;
            transaction.commit()?;
        }
        let value = Self { connection, limits };
        value.status()?;
        Ok(value)
    }

    pub fn status(&self) -> Result<VectorStatus, VectorError> {
        let active: Option<i64> = self.connection.query_row(
            "SELECT active_generation FROM vector_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        let mut statement = self
            .connection
            .prepare("SELECT id FROM vector_generations ORDER BY id LIMIT 3")?;
        let ids = statement
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if ids.len() > 2 {
            return Err(VectorError::Incompatible);
        }
        let mut result = VectorStatus {
            active: None,
            building: None,
            last_error: self.connection.query_row(
                "SELECT last_error,coalesce(length(CAST(last_error AS BLOB)),0) FROM vector_state WHERE singleton=1",
                [],
                |row| { let length:usize=row.get(1)?; if length>1024 {return Err(rusqlite::Error::InvalidQuery);} row.get(0) },
            )?,
        };
        for id in ids {
            let generation = self.generation(id)?;
            if Some(id) == active {
                if !generation.ready {
                    return Err(VectorError::Incompatible);
                }
                result.active = Some(generation);
            } else if !generation.ready {
                if result.building.is_some() {
                    return Err(VectorError::Incompatible);
                }
                result.building = Some(generation);
            }
        }
        if active.is_some() && result.active.is_none() {
            return Err(VectorError::Incompatible);
        }
        Ok(result)
    }

    pub(crate) fn prepare(
        &mut self,
        spec: &EmbeddingIndexSpec,
        source: &str,
        force: bool,
    ) -> Result<GenerationStatus, VectorError> {
        validate_profile(spec.profile())?;
        validate_digest(source)?;
        let status = self.status()?;
        if !force {
            if let Some(active) = &status.active
                && active.spec == *spec
                && active.source_fingerprint == source
            {
                return Ok(active.clone());
            }
            if let Some(building) = &status.building
                && building.spec == *spec
                && building.source_fingerprint == source
            {
                return Ok(building.clone());
            }
        }
        let transaction = self.connection.transaction()?;
        let active = status.active.as_ref().map(|value| value.id);
        let ids = {
            let mut statement = transaction.prepare("SELECT id FROM vector_generations")?;
            statement
                .query_map([], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        for id in ids {
            if Some(id) != active {
                transaction.execute_batch(&format!("DROP TABLE {};", table_name(id)?))?;
                transaction.execute("DELETE FROM vector_generations WHERE id=?1", [id])?;
            }
        }
        let encoded = serde_json::to_string(spec)?;
        if encoded.len() > 32_768 {
            return Err(VectorError::Limit("specification bytes"));
        }
        transaction.execute("INSERT INTO vector_generations(fingerprint,spec,source_fingerprint,ready,vectors,embedded,reused,skipped_blank) VALUES(?1,?2,?3,0,0,0,0,0)", params![spec.fingerprint().as_str(),encoded,source])?;
        let id = transaction.last_insert_rowid();
        transaction.execute_batch(&format!("CREATE VIRTUAL TABLE {} USING vec0(chunk_id TEXT PRIMARY KEY, embedding float[{}] distance_metric=cosine);", table_name(id)?, spec.profile().dimensions()))?;
        transaction.execute(
            "UPDATE vector_state SET last_error=NULL WHERE singleton=1",
            [],
        )?;
        transaction.commit()?;
        self.generation(id)
    }

    pub(crate) fn record(
        &mut self,
        generation: i64,
        chunk_id: &str,
        input_hash: Option<&str>,
        vector: Option<&EmbeddingVector>,
        reused: bool,
    ) -> Result<(), VectorError> {
        validate_digest(chunk_id)?;
        let current = self.generation(generation)?;
        if current.ready
            || current
                .after_chunk_id
                .as_deref()
                .is_some_and(|after| chunk_id <= after)
        {
            return Err(VectorError::Incompatible);
        }
        if let Some(vector) = vector {
            if current.vectors >= self.limits.max_vectors {
                return Err(VectorError::Limit("generation vectors"));
            }
            current
                .spec
                .profile()
                .ensure_compatible(vector.profile_fingerprint())?;
            current.spec.profile().validate_vector(vector.values())?;
        }
        let transaction = self.connection.transaction()?;
        if let (Some(hash), Some(vector)) = (input_hash, vector) {
            validate_digest(hash)?;
            let bytes = encode_vector(vector.values());
            transaction.execute(
                &format!(
                    "INSERT INTO {}(chunk_id,embedding) VALUES(?1,?2)",
                    table_name(generation)?
                ),
                params![chunk_id, bytes],
            )?;
            transaction.execute(
                "INSERT INTO vector_inputs(generation,chunk_id,input_hash) VALUES(?1,?2,?3)",
                params![generation, chunk_id, hash],
            )?;
        } else if input_hash.is_some() || vector.is_some() {
            return Err(VectorError::Incompatible);
        }
        transaction.execute("UPDATE vector_generations SET after_chunk_id=?2,vectors=vectors+?3,embedded=embedded+?4,reused=reused+?5,skipped_blank=skipped_blank+?6 WHERE id=?1", params![generation,chunk_id,usize::from(vector.is_some()),usize::from(vector.is_some() && !reused),usize::from(vector.is_some() && reused),usize::from(vector.is_none())])?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn reusable(
        &self,
        spec: &EmbeddingIndexSpec,
        chunk: &str,
        input_hash: &str,
    ) -> Result<Option<EmbeddingVector>, VectorError> {
        let Some(active) = self.status()?.active else {
            return Ok(None);
        };
        if active.spec != *spec {
            return Ok(None);
        }
        let mut statement = self.connection.prepare(&format!("SELECT v.embedding,length(v.embedding) FROM {} v JOIN vector_inputs i ON i.chunk_id=v.chunk_id AND i.generation=?1 WHERE v.chunk_id=?2 AND i.input_hash=?3", table_name(active.id)?))?;
        let value = statement
            .query_row(params![active.id, chunk, input_hash], |row| {
                let bytes: usize = row.get(1)?;
                if bytes != spec.profile().dimensions() * 4 {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                row.get::<_, Vec<u8>>(0)
            })
            .optional()?;
        value
            .map(|bytes| decode_vector(spec.profile(), &bytes))
            .transpose()
    }

    pub(crate) fn activate(
        &mut self,
        generation: i64,
        source: &str,
    ) -> Result<GenerationStatus, VectorError> {
        let current = self.generation(generation)?;
        if current.source_fingerprint != source {
            return Err(VectorError::SourceChanged);
        }
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "UPDATE vector_generations SET ready=1 WHERE id=?1",
            [generation],
        )?;
        transaction.execute(
            "UPDATE vector_state SET active_generation=?1,last_error=NULL WHERE singleton=1",
            [generation],
        )?;
        transaction.commit()?;
        self.generation(generation)
    }

    pub(crate) fn nearest(
        &self,
        generation: &GenerationStatus,
        vector: &EmbeddingVector,
        limit: usize,
    ) -> Result<Vec<(String, f64, String)>, VectorError> {
        if !generation.ready || limit == 0 || limit > 100 {
            return Err(VectorError::Invalid("nearest candidate count"));
        }
        generation
            .spec
            .profile()
            .ensure_compatible(vector.profile_fingerprint())?;
        generation.spec.profile().validate_vector(vector.values())?;
        let bytes = encode_vector(vector.values());
        let mut statement = self.connection.prepare(&format!("WITH nearest AS MATERIALIZED (SELECT chunk_id,distance FROM {} WHERE embedding MATCH ?1 AND k=?3 ORDER BY distance) SELECT v.chunk_id,v.distance,i.input_hash,length(CAST(v.chunk_id AS BLOB)),length(CAST(i.input_hash AS BLOB)) FROM nearest v JOIN vector_inputs i ON i.chunk_id=v.chunk_id AND i.generation=?2 ORDER BY v.distance,v.chunk_id", table_name(generation.id)?))?;
        let mut rows = statement.query(params![bytes, generation.id, limit])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            let id_bytes: usize = row.get(3)?;
            let hash_bytes: usize = row.get(4)?;
            if id_bytes != 64 || hash_bytes != 64 {
                return Err(VectorError::Incompatible);
            }
            let id: String = row.get(0)?;
            let distance: f64 = row.get(1)?;
            let hash: String = row.get(2)?;
            validate_digest(&id)?;
            validate_digest(&hash)?;
            if !distance.is_finite() {
                return Err(VectorError::Incompatible);
            }
            result.push((id, distance, hash));
        }
        Ok(result)
    }

    pub(crate) fn failure(&mut self, error: &VectorError) -> Result<(), VectorError> {
        let diagnostic: String = error.to_string().chars().take(256).collect();
        self.connection.execute(
            "UPDATE vector_state SET last_error=?1 WHERE singleton=1",
            [diagnostic],
        )?;
        Ok(())
    }

    fn generation(&self, id: i64) -> Result<GenerationStatus, VectorError> {
        table_name(id)?;
        self.connection.query_row("SELECT spec,source_fingerprint,ready,after_chunk_id,vectors,embedded,reused,skipped_blank,fingerprint,length(CAST(spec AS BLOB)),length(CAST(source_fingerprint AS BLOB)),coalesce(length(CAST(after_chunk_id AS BLOB)),64),length(CAST(fingerprint AS BLOB)) FROM vector_generations WHERE id=?1", [id], |row| {
            let length: usize = row.get(9)?;
            if length > 32_768 { return Err(rusqlite::Error::InvalidQuery); }
            for index in [10,11,12] {let size:usize=row.get(index)?; if size!=64 {return Err(rusqlite::Error::InvalidQuery);}}
            Ok((row.get::<_, String>(0)?,row.get::<_, String>(1)?,row.get::<_, i64>(2)?,row.get::<_, Option<String>>(3)?,row.get::<_, usize>(4)?,row.get::<_, usize>(5)?,row.get::<_, usize>(6)?,row.get::<_, usize>(7)?,row.get::<_, String>(8)?))
        }).map_err(VectorError::from).and_then(|row| {
            if !matches!(row.2,0|1) {return Err(VectorError::Incompatible);}
            let spec: EmbeddingIndexSpec = serde_json::from_str(&row.0)?;
            validate_profile(spec.profile())?;
            validate_digest(&row.1)?;
            if let Some(cursor) = &row.3 { validate_digest(cursor)?; }
            if row.4 > self.limits.max_vectors || spec.fingerprint().as_str() != row.8 || row.5.saturating_add(row.6) != row.4 { return Err(VectorError::Incompatible); }
            Ok(GenerationStatus { id,spec,source_fingerprint:row.1,ready:row.2==1,after_chunk_id:row.3,vectors:row.4,embedded:row.5,reused:row.6,skipped_blank:row.7 })
        })
    }
}

pub(crate) fn checkpoint(cancellation: &CancellationToken) -> Result<(), VectorError> {
    if cancellation.is_cancelled() {
        Err(VectorError::Cancelled)
    } else {
        Ok(())
    }
}

pub(crate) fn validate_profile(profile: &EmbeddingProfile) -> Result<(), VectorError> {
    if ![128, 256, 512, 768].contains(&profile.dimensions())
        || profile.spec().metric() != EmbeddingMetric::Cosine
        || profile.spec().normalization() != EmbeddingNormalization::UnitL2
    {
        return Err(VectorError::Invalid(
            "initial vector profiles require 128/256/512/768 unit-L2 cosine dimensions",
        ));
    }
    Ok(())
}

fn validate_digest(value: &str) -> Result<(), VectorError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|value| value.is_ascii_digit() || (b'a'..=b'f').contains(&value))
    {
        return Err(VectorError::Incompatible);
    }
    Ok(())
}

fn table_name(id: i64) -> Result<String, VectorError> {
    if id <= 0 {
        return Err(VectorError::Incompatible);
    }
    Ok(format!("wiki_vec_{id}"))
}

fn encode_vector(values: &[f32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect()
}

fn decode_vector(profile: &EmbeddingProfile, bytes: &[u8]) -> Result<EmbeddingVector, VectorError> {
    if bytes.len() != profile.dimensions() * 4 {
        return Err(VectorError::Incompatible);
    }
    let values = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|value| f32::from_ne_bytes(*value))
        .collect();
    Ok(EmbeddingVector::new(profile, values)?)
}

type ExtensionInit = unsafe extern "C" fn(
    *mut ffi::sqlite3,
    *mut *mut c_char,
    *const ffi::sqlite3_api_routines,
) -> c_int;

fn initialize_extension(connection: &mut Connection) -> Result<(), VectorError> {
    if cfg!(target_endian = "big") {
        return Err(VectorError::Invalid(
            "initial sqlite-vec storage requires little-endian hosts",
        ));
    }
    let mut error = std::ptr::null_mut();
    let result = unsafe {
        let initialize: ExtensionInit = std::mem::transmute::<unsafe extern "C" fn(), ExtensionInit>(
            sqlite_vec::sqlite3_vec_init,
        );
        let result = initialize(connection.handle(), &mut error, std::ptr::null());
        if !error.is_null() {
            ffi::sqlite3_free(error.cast());
        }
        result
    };
    if result != ffi::SQLITE_OK {
        return Err(VectorError::Database(rusqlite::Error::SqliteFailure(
            ffi::Error::new(result),
            None,
        )));
    }
    Ok(())
}

//! Canonical legacy insertion request. Caller choices, not generated results,
//! define retry identity. This does not activate schema-v2 serving or public CAS.
use crate::model::{MemoryId, Tag};
use crate::storage::{StorageError, control};

#[derive(Debug, Clone, serde::Serialize)]
pub struct InsertMemory {
    /// None requests a generated UUID; Some is a caller-supplied identity.
    pub id: Option<MemoryId>,
    pub summary: String,
    pub full_text: Option<String>,
    pub tags: Vec<Tag>,
    /// None is the normalized instruction "use namespace default at commit".
    pub initial_stability: Option<f32>,
    /// None is the normalized instruction "use server clock at commit".
    pub created_at: Option<i64>,
    /// Supplied bytes participate in identity. None requests provider generation;
    /// the separately prepared provider result is never request identity.
    pub embedding: Option<Vec<f32>>,
    pub parent_id: Option<MemoryId>,
    pub supersedes: Option<MemoryId>,
}

impl InsertMemory {
    pub(crate) fn request_hash(
        &self,
        ctx: &control::MutationContext,
        initial_stability: f32,
        embedding_dim: u32,
    ) -> Result<[u8; 32], StorageError> {
        // Encode floats as exact bits; JSON float normalization must not change
        // caller identity. Ordered tags retain their normalized caller order.
        let fields = control::encode(&(
            self.id,
            &self.summary,
            &self.full_text,
            &self.tags,
            self.initial_stability.map(f32::to_bits),
            self.created_at,
            self.embedding
                .as_ref()
                .map(|v| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>()),
            self.parent_id,
            self.supersedes,
            // Expanded defaults are hashed in addition to caller intent.
            // Difficulty/phase/strength are fixed by contract version 1.
            initial_stability.to_bits(),
            embedding_dim,
            5.0f32.to_bits(),
            "Full",
            1.0f32.to_bits(),
        ))?;
        Ok(control::request_hash(ctx, "memory_insert_v1", &[&fields]))
    }
}

/// In-memory fresh/current response only. The durable receipt never embeds a
/// record. A keyed replay after deletion may have no current record.
pub struct InsertResult {
    pub receipt: control::WriteReceipt,
    pub record: Option<crate::model::CachedRecord>,
}

pub fn inserted_id(receipt: &control::WriteReceipt) -> Result<MemoryId, StorageError> {
    match receipt.outcome {
        control::MutationOutcome::MemoryInserted { memory_id, .. } => Ok(memory_id),
        _ => Err(StorageError::InvalidControl(
            "expected insertion receipt".into(),
        )),
    }
}

/// Transport working vectors and final canonical staging use the same numeric
/// boundary. Keep original supplied bytes in InsertMemory for request identity.
/// Invalid generated output is unavailable, not caller input or store corruption.
pub(crate) fn normalize_embedding(
    embedding: &mut [f32],
    dimensions: u32,
    supplied: bool,
) -> Result<(), StorageError> {
    crate::embedding::validation::normalize_vector(embedding, dimensions as usize).map_err(
        |message| {
            if supplied {
                StorageError::InvalidMutation(message)
            } else {
                StorageError::ProviderVectorUnavailable(message)
            }
        },
    )
}

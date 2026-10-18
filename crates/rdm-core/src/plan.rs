//! Chunk geometry: fixed-size units of work handed out dynamically to workers.
//!
//! Chunk size follows architecture §3.3:
//! `chunk_size.unwrap_or(clamp(total / (connections * 4), MIN, DEFAULT))`.
//! Work stealing is just an atomic `fetch_add` over the chunk index, so a slow
//! connection only ever delays the chunk it currently holds.

use crate::{DEFAULT_CHUNK_SIZE, MIN_CHUNK_SIZE};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChunkPlan {
    pub total: u64,
    pub chunk_size: u64,
    pub total_chunks: u64,
}

impl ChunkPlan {
    /// Derive the plan for a fresh transfer.
    pub fn new(total: u64, connections: usize, requested: Option<u64>) -> Self {
        let connections = connections.max(1) as u64;
        let chunk_size = requested
            .unwrap_or_else(|| (total / (connections * 4)).clamp(MIN_CHUNK_SIZE, DEFAULT_CHUNK_SIZE))
            .max(1);
        Self::with_chunk_size(total, chunk_size)
    }

    /// Build a plan with an explicit chunk size (used to adopt resume state).
    pub fn with_chunk_size(total: u64, chunk_size: u64) -> Self {
        let chunk_size = chunk_size.max(1);
        let total_chunks = if total == 0 {
            0
        } else {
            total.div_ceil(chunk_size)
        };
        Self {
            total,
            chunk_size,
            total_chunks,
        }
    }

    /// Byte length of a chunk (last chunk may be shorter).
    pub fn chunk_len(&self, index: u64) -> u64 {
        if index >= self.total_chunks {
            return 0;
        }
        (self.total - index * self.chunk_size).min(self.chunk_size)
    }

    /// Half-open byte range `[start, end)` of a chunk.
    pub fn byte_range(&self, index: u64) -> (u64, u64) {
        let start = index * self.chunk_size;
        (start, start + self.chunk_len(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_chunk_size_from_connections() {
        // 64 MiB / (8 * 4) = 2 MiB.
        let plan = ChunkPlan::new(64 * 1024 * 1024, 8, None);
        assert_eq!(plan.chunk_size, 2 * 1024 * 1024);
        assert_eq!(plan.total_chunks, 32);
    }

    #[test]
    fn clamps_chunk_size() {
        let small = ChunkPlan::new(1024, 8, None);
        assert_eq!(small.chunk_size, MIN_CHUNK_SIZE);
        assert_eq!(small.total_chunks, 1);

        let large = ChunkPlan::new(4 * 1024 * 1024 * 1024, 1, None);
        assert_eq!(large.chunk_size, DEFAULT_CHUNK_SIZE);
    }

    #[test]
    fn honours_requested_chunk_size() {
        let plan = ChunkPlan::new(10, 4, Some(3));
        assert_eq!(plan.chunk_size, 3);
        assert_eq!(plan.total_chunks, 4);
        assert_eq!(plan.chunk_len(0), 3);
        assert_eq!(plan.chunk_len(3), 1);
        assert_eq!(plan.byte_range(1), (3, 6));
        assert_eq!(plan.byte_range(3), (9, 10));
    }

    #[test]
    fn empty_file_has_no_chunks() {
        let plan = ChunkPlan::new(0, 8, None);
        assert_eq!(plan.total_chunks, 0);
        assert_eq!(plan.byte_range(0), (0, 0));
    }
}

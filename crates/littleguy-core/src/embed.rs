//! Embeddings: the vectors that the memory search ranks by (ADR 0047).
//!
//! An [`Embedder`] turns texts into vectors of one fixed length. The
//! memory store embeds each new fact and episode, and the `recall` tool
//! embeds its query. Two vectors that point the same way hold texts of a
//! like meaning.
//!
//! [`HashingEmbedder`] needs no network and no model. It hashes each word
//! and each pair of words into a slot. So it finds shared words, not
//! meaning. It runs when the config names no endpoint, and in tests.

use std::fmt::Debug;

use autumn_harvest_agent::AgentError;
use autumn_harvest_agent::model::BoxFuture;

/// The dimensions of [`HashingEmbedder`].
pub const HASHING_DIMENSIONS: u32 = 256;

/// The name that [`HashingEmbedder`] records in the store.
pub const HASHING_NAME: &str = "hashing";

/// Turns texts into vectors.
pub trait Embedder: Send + Sync + Debug {
    /// The name of the model. The store records it with the vectors, so
    /// it can tell when the model changes.
    fn name(&self) -> &str;

    /// The length of each vector.
    fn dimensions(&self) -> u32;

    /// One vector for each text, in the same order.
    ///
    /// # Errors
    ///
    /// Returns an error when the model cannot answer, or answers with a
    /// vector of another length.
    fn embed<'a>(&'a self, texts: &'a [String])
    -> BoxFuture<'a, Result<Vec<Vec<f32>>, AgentError>>;
}

/// An embedder that hashes words and word pairs, with no network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashingEmbedder {
    dimensions: u32,
}

impl Default for HashingEmbedder {
    fn default() -> Self {
        Self {
            dimensions: HASHING_DIMENSIONS,
        }
    }
}

impl HashingEmbedder {
    /// An embedder of `dimensions` slots. Zero counts as one.
    #[must_use]
    pub const fn new(dimensions: u32) -> Self {
        Self {
            dimensions: if dimensions == 0 { 1 } else { dimensions },
        }
    }

    /// The vector of one text. It has length 1, so the dot product of two
    /// vectors is their cosine.
    #[must_use]
    pub fn vector(&self, text: &str) -> Vec<f32> {
        let size = self.dimensions as usize;
        let mut vector = vec![0.0_f32; size];
        let words: Vec<String> = text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .collect();
        for word in &words {
            add(&mut vector, word, 1.0);
        }
        for pair in words.windows(2) {
            add(&mut vector, &format!("{} {}", pair[0], pair[1]), 0.5);
        }
        normalize(&mut vector);
        vector
    }
}

/// Adds `weight` to the slot of `token`, with a sign from the hash, so
/// two tokens in one slot can cancel instead of always adding up.
fn add(vector: &mut [f32], token: &str, weight: f32) {
    let hash = fnv1a(token.as_bytes());
    let len = vector.len() as u64;
    // The modulus is below the length, which is a usize.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the slot is below the vector length, a usize"
    )]
    let slot = (hash % len) as usize;
    let sign: f32 = if hash & (1 << 63) == 0 { 1.0 } else { -1.0 };
    vector[slot] = sign.mul_add(weight, vector[slot]);
}

/// Scales `vector` to length 1. A vector with no token gets its first
/// slot set, so it is never all zeros and its cosine stays defined.
fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in vector.iter_mut() {
            *x /= norm;
        }
    } else if let Some(first) = vector.first_mut() {
        *first = 1.0;
    }
}

/// The 64-bit FNV-1a hash. It is stable across builds, so a stored vector
/// keeps its meaning after an upgrade.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

impl Embedder for HashingEmbedder {
    fn name(&self) -> &'static str {
        HASHING_NAME
    }

    fn dimensions(&self) -> u32 {
        self.dimensions
    }

    fn embed<'a>(
        &'a self,
        texts: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<Vec<f32>>, AgentError>> {
        let vectors = texts.iter().map(|text| self.vector(text)).collect();
        Box::pin(std::future::ready(Ok(vectors)))
    }
}

/// The cosine of two vectors of the same length. Zero when either one is
/// all zeros or the lengths differ.
#[must_use]
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn shared_words_score_higher_than_other_words() {
        let embedder = HashingEmbedder::default();
        let query = embedder.vector("what tea does the owner like");
        let tea = embedder.vector("The owner likes green tea in the morning");
        let car = embedder.vector("The car needs new tyres before winter");
        assert!(cosine(&query, &tea) > cosine(&query, &car));
    }

    #[test]
    fn the_vector_ignores_case_and_punctuation() {
        let embedder = HashingEmbedder::default();
        let a = embedder.vector("Green tea!");
        let b = embedder.vector("green, TEA");
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn an_empty_text_has_a_defined_vector() {
        let embedder = HashingEmbedder::new(8);
        let vector = embedder.vector("  ...  ");
        assert_eq!(vector.len(), 8);
        assert!((vector[0] - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn zero_dimensions_count_as_one() {
        assert_eq!(HashingEmbedder::new(0).dimensions(), 1);
        assert_eq!(HashingEmbedder::new(0).vector("tea").len(), 1);
    }

    #[test]
    fn cosine_of_unequal_or_zero_vectors_is_zero() {
        assert!(cosine(&[1.0], &[1.0, 0.0]).abs() < f32::EPSILON);
        assert!(cosine(&[0.0, 0.0], &[1.0, 0.0]).abs() < f32::EPSILON);
    }

    #[test]
    fn the_hash_is_stable() {
        // A stored vector must keep its meaning after an upgrade.
        assert_eq!(fnv1a(b"tea"), 0x56e4_c919_4453_6249);
    }

    #[test]
    fn embed_answers_one_vector_per_text_in_order() {
        let embedder = HashingEmbedder::new(16);
        let texts = vec!["tea".to_owned(), "car".to_owned()];
        let runtime = tokio_free_block_on(embedder.embed(&texts));
        let vectors = runtime.unwrap_or_default();
        assert_eq!(vectors.len(), 2);
        assert_eq!(vectors[0], embedder.vector("tea"));
        assert_eq!(embedder.name(), HASHING_NAME);
    }

    /// Polls a ready future once. The hashing embedder never waits.
    fn tokio_free_block_on<T>(
        future: BoxFuture<'_, Result<T, AgentError>>,
    ) -> Result<T, AgentError> {
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        let mut future = future;
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(value) => value,
            std::task::Poll::Pending => Err(AgentError::new(
                autumn_harvest_agent::ErrorKind::Unavailable,
                "the hashing embedder waited",
            )),
        }
    }

    proptest! {
        #[test]
        fn every_vector_has_length_one(text in ".{0,80}", dims in 1_u32..64) {
            let vector = HashingEmbedder::new(dims).vector(&text);
            let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
            prop_assert_eq!(vector.len(), dims as usize);
            prop_assert!((norm - 1.0).abs() < 1e-4);
        }

        #[test]
        fn a_text_is_closest_to_itself(text in "[a-z ]{1,40}") {
            let embedder = HashingEmbedder::default();
            let vector = embedder.vector(&text);
            prop_assert!((cosine(&vector, &vector) - 1.0).abs() < 1e-4);
        }
    }
}

//! Deterministic RNG streams. No shared RNG anywhere: every random decision
//! draws from a stream derived from the master seed with a stable key.
//!
//! Two key families — do not mix them up (DESIGN.md, Determinism):
//! - **transport**: keyed by (seed, claim_id, attempt, decision_point).
//!   Retries get fresh fates; otherwise a dropped claim is dropped forever.
//! - **adjudication**: keyed by (seed, claim_id, decision_point) — NO attempt.
//!   Re-delivery reproduces the identical remittance, which is what makes
//!   stateless payers sound.

use rand_chacha::ChaCha8Rng;
use rand_chacha::rand_core::SeedableRng;

use crate::domain::ClaimId;

#[derive(Clone, Copy, Debug)]
pub struct RngFactory {
    seed: u64,
}

impl RngFactory {
    pub fn new(seed: u64) -> RngFactory {
        RngFactory { seed }
    }

    pub fn transport(&self, claim_id: &ClaimId, attempt: u32, decision_point: &str) -> ChaCha8Rng {
        self.stream(&[
            b"transport",
            claim_id.0.as_bytes(),
            &attempt.to_le_bytes(),
            decision_point.as_bytes(),
        ])
    }

    pub fn adjudication(&self, claim_id: &ClaimId, decision_point: &str) -> ChaCha8Rng {
        self.stream(&[
            b"adjudication",
            claim_id.0.as_bytes(),
            decision_point.as_bytes(),
        ])
    }

    fn stream(&self, parts: &[&[u8]]) -> ChaCha8Rng {
        ChaCha8Rng::seed_from_u64(hash_parts(self.seed, parts))
    }
}

/// FNV-1a over length-prefixed parts: stable across builds (unlike
/// `DefaultHasher`) and unambiguous under concatenation.
fn hash_parts(seed: u64, parts: &[&[u8]]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut h = OFFSET ^ seed;
    let mut eat = |bytes: &[u8]| {
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(PRIME);
        }
    };
    for part in parts {
        eat(&(part.len() as u64).to_le_bytes());
        eat(part);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::Rng;

    fn first_draw(rng: &mut ChaCha8Rng) -> u64 {
        rng.random()
    }

    #[test]
    fn same_key_same_stream() {
        let f = RngFactory::new(42);
        let id = ClaimId("c-1".into());
        let a = first_draw(&mut f.transport(&id, 1, "drop"));
        let b = first_draw(&mut f.transport(&id, 1, "drop"));
        assert_eq!(a, b);
    }

    #[test]
    fn attempt_changes_transport_fate_but_not_adjudication() {
        let f = RngFactory::new(42);
        let id = ClaimId("c-1".into());
        assert_ne!(
            first_draw(&mut f.transport(&id, 1, "drop")),
            first_draw(&mut f.transport(&id, 2, "drop")),
        );
        // Adjudication has no attempt axis at all — the API makes the
        // "identical remittance on re-delivery" property structural.
        assert_eq!(
            first_draw(&mut f.adjudication(&id, "line/L1")),
            first_draw(&mut f.adjudication(&id, "line/L1")),
        );
    }

    #[test]
    fn families_seed_claim_and_point_all_separate_streams() {
        let f = RngFactory::new(42);
        let id = ClaimId("c-1".into());
        let base = first_draw(&mut f.adjudication(&id, "latency"));
        assert_ne!(base, first_draw(&mut f.transport(&id, 1, "latency")));
        assert_ne!(
            base,
            first_draw(&mut f.adjudication(&ClaimId("c-2".into()), "latency"))
        );
        assert_ne!(base, first_draw(&mut f.adjudication(&id, "line/L1")));
        assert_ne!(
            base,
            first_draw(&mut RngFactory::new(43).adjudication(&id, "latency"))
        );
    }
}

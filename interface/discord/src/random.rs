//! Randomness backed by nightly `std::random`.

use std::{
    num::NonZeroU64,
    random::{DefaultRandomSource, Distribution as _, RandomSource as _},
};

use rsa::rand_core::{CryptoRng, Error, RngCore};

/// System-backed randomness with helpers for Discord's required distributions.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SystemRandom(DefaultRandomSource);

impl SystemRandom {
    /// Samples uniformly from `[0, 1)` using all 53 significant `f64` bits.
    pub(crate) fn unit_interval(mut self) -> f64 {
        const SIGNIFICAND_BITS: u32 = 53;
        const SCALE: f64 = 1.0 / ((1_u64 << SIGNIFICAND_BITS) as f64);

        let significant = self.next_u64() >> (u64::BITS - SIGNIFICAND_BITS);
        (significant as f64) * SCALE
    }

    /// Samples uniformly from `[0, exclusive_upper)` without modulo bias.
    pub(crate) fn below(mut self, exclusive_upper: NonZeroU64) -> u64 {
        let exclusive_upper = exclusive_upper.get();
        let rejection_threshold = exclusive_upper.wrapping_neg() % exclusive_upper;
        loop {
            let candidate = self.next_u64();
            if candidate >= rejection_threshold {
                return candidate % exclusive_upper;
            }
        }
    }
}

impl RngCore for SystemRandom {
    fn next_u32(&mut self) -> u32 {
        (..).sample(&mut self.0)
    }

    fn next_u64(&mut self) -> u64 {
        (..).sample(&mut self.0)
    }

    fn fill_bytes(&mut self, destination: &mut [u8]) {
        self.0.fill_bytes(destination);
    }

    fn try_fill_bytes(&mut self, destination: &mut [u8]) -> Result<(), Error> {
        self.0.fill_bytes(destination);
        Ok(())
    }
}

// `DefaultRandomSource` documents its system entropy as suitable for
// cryptographic purposes, matching `CryptoRng`'s semantic contract.
impl CryptoRng for SystemRandom {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_distributions_stay_within_bounds() {
        let upper = NonZeroU64::new(5).expect("test bound is nonzero");
        for _ in 0..128 {
            assert!((0.0..1.0).contains(&SystemRandom::default().unit_interval()));
            assert!(SystemRandom::default().below(upper) < upper.get());
        }
    }

    #[test]
    fn system_random_satisfies_rsa_crypto_rng_contract() {
        fn require_crypto_rng(_: &mut impl rsa::rand_core::CryptoRngCore) {}

        let mut random = SystemRandom::default();
        require_crypto_rng(&mut random);
        let mut bytes = [0_u8; 32];
        random.fill_bytes(&mut bytes);
    }
}

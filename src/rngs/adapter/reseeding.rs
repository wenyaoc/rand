// Copyright 2018 Developers of the Rand project.
// Copyright 2013 The Rust Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A wrapper around another PRNG that reseeds it after it
//! generates a certain number of random bytes.

use core::mem::size_of;

use rand_core::{RngCore, CryptoRng, SeedableRng, Error};
use rand_core::block::{BlockRngCore, BlockRng};

/// A wrapper around any PRNG that implements [`BlockRngCore`], that adds the
/// ability to reseed it.
///
/// `ReseedingRng` reseeds the underlying PRNG in the following cases:
///
/// - On a manual call to [`reseed()`].
/// - After `clone()`, the clone will be reseeded on first use.
/// - After a process is forked, the RNG in the child process is reseeded within
///   the next few generated values, depending on the block size of the
///   underlying PRNG. For ChaCha and Hc128 this is a maximum of
///   15 `u32` values before reseeding.
/// - After the PRNG has generated a configurable number of random bytes.
///
/// # When should reseeding after a fixed number of generated bytes be used?
///
/// Reseeding after a fixed number of generated bytes is never strictly
/// *necessary*. Cryptographic PRNGs don't have a limited number of bytes they
/// can output, or at least not a limit reachable in any practical way. There is
/// no such thing as 'running out of entropy'.
///
/// Occasionally reseeding can be seen as some form of 'security in depth'. Even
/// if in the future a cryptographic weakness is found in the CSPRNG being used,
/// or a flaw in the implementation, occasionally reseeding should make
/// exploiting it much more difficult or even impossible.
///
/// Use [`ReseedingRng::new`] with a `threshold` of `0` to disable reseeding
/// after a fixed number of generated bytes.
///
/// # Error handling
///
/// Although unlikely, reseeding the wrapped PRNG can fail. `ReseedingRng` will
/// never panic but try to handle the error intelligently through some
/// combination of retrying and delaying reseeding until later.
/// If handling the source error fails `ReseedingRng` will continue generating
/// data from the wrapped PRNG without reseeding.
///
/// Manually calling [`reseed()`] will not have this retry or delay logic, but
/// reports the error.
///
/// # Example
///
/// ```
/// # extern crate rand;
/// # extern crate rand_chacha;
/// # fn main() {
/// use rand::prelude::*;
/// use rand_chacha::ChaCha20Core; // Internal part of ChaChaRng that
///                              // implements BlockRngCore
/// use rand::rngs::OsRng;
/// use rand::rngs::adapter::ReseedingRng;
///
/// let prng = ChaCha20Core::from_entropy();
/// let mut reseeding_rng = ReseedingRng::new(prng, 0, OsRng);
///
/// println!("{}", reseeding_rng.gen::<u64>());
///
/// let mut cloned_rng = reseeding_rng.clone();
/// assert!(reseeding_rng.gen::<u64>() != cloned_rng.gen::<u64>());
/// # }
/// ```
///
/// [`ChaCha20Core`]: ../../../rand_chacha/struct.ChaCha20Core.html
/// [`BlockRngCore`]: rand_core::block::BlockRngCore
/// [`ReseedingRng::new`]: ReseedingRng::new
/// [`reseed()`]: ReseedingRng::reseed
#[derive(Debug)]
pub struct ReseedingRng<R, Rsdr>(BlockRng<ReseedingCore<R, Rsdr>>)
where R: BlockRngCore + SeedableRng,
      Rsdr: RngCore;

impl<R, Rsdr> ReseedingRng<R, Rsdr>
where R: BlockRngCore + SeedableRng,
      Rsdr: RngCore
{
    /// Create a new `ReseedingRng` from an existing PRNG, combined with a RNG
    /// to use as reseeder.
    ///
    /// `threshold` sets the number of generated bytes after which to reseed the
    /// PRNG. Set it to zero to never reseed based on the number of generated
    /// values.
    pub fn new(rng: R, threshold: u64, reseeder: Rsdr) -> Self {
        ReseedingRng(BlockRng::new(ReseedingCore::new(rng, threshold, reseeder)))
    }

    /// Reseed the internal PRNG.
    pub fn reseed(&mut self) -> Result<(), Error> {
        self.0.core.reseed()
    }
}

// TODO: this should be implemented for any type where the inner type
// implements RngCore, but we can't specify that because ReseedingCore is private
impl<R, Rsdr: RngCore> RngCore for ReseedingRng<R, Rsdr>
where R: BlockRngCore<Item = u32> + SeedableRng,
    <R as BlockRngCore>::Results: AsRef<[u32]> + AsMut<[u32]>
{
    #[inline(always)]
    fn next_u32(&mut self) -> u32 {
        self.0.next_u32()
    }

    #[inline(always)]
    fn next_u64(&mut self) -> u64 {
        self.0.next_u64()
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.0.fill_bytes(dest)
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Error> {
        self.0.try_fill_bytes(dest)
    }
}

impl<R, Rsdr> Clone for ReseedingRng<R, Rsdr>
where R: BlockRngCore + SeedableRng + Clone,
      Rsdr: RngCore + Clone
{
    fn clone(&self) -> ReseedingRng<R, Rsdr> {
        // Recreating `BlockRng` seems easier than cloning it and resetting
        // the index.
        ReseedingRng(BlockRng::new(self.0.core.clone()))
    }
}

impl<R, Rsdr> CryptoRng for ReseedingRng<R, Rsdr>
where R: BlockRngCore + SeedableRng + CryptoRng,
      Rsdr: RngCore + CryptoRng {}

#[derive(Debug)]
struct ReseedingCore<R, Rsdr> {
    inner: R,
    reseeder: Rsdr,
    threshold: i64,
    bytes_until_reseed: i64,
    fork_counter: usize,
}

impl<R, Rsdr> BlockRngCore for ReseedingCore<R, Rsdr>
where R: BlockRngCore + SeedableRng,
      Rsdr: RngCore
{
    type Item = <R as BlockRngCore>::Item;
    type Results = <R as BlockRngCore>::Results;

    fn generate(&mut self, results: &mut Self::Results) {
        let global_fork_counter = fork::get_fork_counter();
        if self.bytes_until_reseed <= 0 ||
           self.is_forked(global_fork_counter) {
            // We get better performance by not calling only `reseed` here
            // and continuing with the rest of the function, but by directly
            // returning from a non-inlined function.
            return self.reseed_and_generate(results, global_fork_counter);
        }
        let num_bytes = results.as_ref().len() * size_of::<Self::Item>();
        self.bytes_until_reseed -= num_bytes as i64;
        self.inner.generate(results);
    }
}

impl<R, Rsdr> ReseedingCore<R, Rsdr>
where R: BlockRngCore + SeedableRng,
      Rsdr: RngCore
{
    /// Create a new `ReseedingCore`.
    fn new(rng: R, threshold: u64, reseeder: Rsdr) -> Self {
        use ::core::i64::MAX;
        fork::register_fork_handler();

        // Because generating more values than `i64::MAX` takes centuries on
        // current hardware, we just clamp to that value.
        // Also we set a threshold of 0, which indicates no limit, to that
        // value.
        let threshold =
            if threshold == 0 { MAX }
            else if threshold <= MAX as u64 { threshold as i64 }
            else { MAX };

        ReseedingCore {
            inner: rng,
            reseeder,
            threshold: threshold as i64,
            bytes_until_reseed: threshold as i64,
            fork_counter: 0,
        }
    }

    /// Reseed the internal PRNG.
    fn reseed(&mut self) -> Result<(), Error> {
        R::from_rng(&mut self.reseeder).map(|result| {
            self.bytes_until_reseed = self.threshold;
            self.inner = result
        })
    }

    fn is_forked(&self, global_fork_counter: usize) -> bool {
        // In theory, on 32-bit platforms, it is possible for
        // `global_fork_counter` to wrap around after ~4e9 forks.
        //
        // This check will detect a fork in the normal case where
        // `fork_counter < global_fork_counter`, and also when the difference
        // between both is greater than `isize::MAX` (wrapped around).
        //
        // It will still fail to detect a fork if there have been more than
        // `isize::MAX` forks, without any reseed in between. Seems unlikely
        // enough.
        (self.fork_counter.wrapping_sub(global_fork_counter) as isize) < 0
    }

    #[inline(never)]
    fn reseed_and_generate(&mut self,
                           results: &mut <Self as BlockRngCore>::Results,
                           global_fork_counter: usize)
    {
        if self.is_forked(global_fork_counter) {
            info!("Fork detected, reseeding RNG");
        } else {
            trace!("Reseeding RNG (periodic reseed)");
        }

        let num_bytes =
            results.as_ref().len() * size_of::<<R as BlockRngCore>::Item>();

        if let Err(e) = self.reseed() {
            warn!("Reseeding RNG failed: {}", e);
            let _ = e;
        }
        self.fork_counter = global_fork_counter;

        self.bytes_until_reseed = self.threshold - num_bytes as i64;
        self.inner.generate(results);
    }
}

impl<R, Rsdr> Clone for ReseedingCore<R, Rsdr>
where R: BlockRngCore + SeedableRng + Clone,
      Rsdr: RngCore + Clone
{
    fn clone(&self) -> ReseedingCore<R, Rsdr> {
        ReseedingCore {
            inner: self.inner.clone(),
            reseeder: self.reseeder.clone(),
            threshold: self.threshold,
            bytes_until_reseed: 0, // reseed clone on first use
            fork_counter: self.fork_counter,
        }
    }
}

impl<R, Rsdr> CryptoRng for ReseedingCore<R, Rsdr>
where R: BlockRngCore + SeedableRng + CryptoRng,
      Rsdr: RngCore + CryptoRng {}


#[cfg(all(unix, not(target_os="emscripten")))]
mod fork {
    extern crate libc;

    use core::sync::atomic::{AtomicUsize, AtomicBool, Ordering};
    #[allow(deprecated)]  // Required for compatibility with Rust < 1.24.
    use core::sync::atomic::{ATOMIC_USIZE_INIT, ATOMIC_BOOL_INIT};

    // Fork protection
    //
    // We implement fork protection on Unix using `pthread_atfork`.
    // When the process is forked, we increment `RESEEDING_RNG_FORK_COUNTER`.
    // Every `ReseedingRng` stores the last known value of the static in
    // `fork_counter`. If the cached `fork_counter` is less than
    // `RESEEDING_RNG_FORK_COUNTER`, it is time to reseed this RNG.
    //
    // If reseeding fails, we don't deal with this by setting a delay, but just
    // don't update `fork_counter`, so a reseed is attempted as soon as
    // possible.

    #[allow(deprecated)]
    static RESEEDING_RNG_FORK_COUNTER: AtomicUsize = ATOMIC_USIZE_INIT;

    pub fn get_fork_counter() -> usize {
        RESEEDING_RNG_FORK_COUNTER.load(Ordering::Relaxed)
    }

    #[allow(deprecated)]
    static FORK_HANDLER_REGISTERED: AtomicBool = ATOMIC_BOOL_INIT;

    extern fn fork_handler() {
        // Note: fetch_add is defined to wrap on overflow
        // (which is what we want).
        RESEEDING_RNG_FORK_COUNTER.fetch_add(1, Ordering::Relaxed);
    }

    pub fn register_fork_handler() {
        if FORK_HANDLER_REGISTERED.load(Ordering::Relaxed) == false {
            unsafe { libc::pthread_atfork(None, None, Some(fork_handler)) };
            FORK_HANDLER_REGISTERED.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(not(all(unix, not(target_os="emscripten"))))]
mod fork {
    pub fn get_fork_counter() -> usize { 0 }
    pub fn register_fork_handler() {}
}


#[cfg(test)]
mod test {
    use {Rng, SeedableRng};
    use rand_chacha::ChaCha8Core;
    use rngs::mock::StepRng;
    use super::ReseedingRng;

    #[test]
    fn test_reseeding() {
        let mut zero = StepRng::new(0, 0);
        let rng = ChaCha8Core::from_rng(&mut zero).unwrap();
        let thresh = 1; // reseed every time the buffer is exhausted
        let mut reseeding = ReseedingRng::new(rng, thresh, zero);

        // RNG buffer size is [u32; 64]
        // Debug is only implemented up to length 32 so use two arrays
        let mut buf = ([0u32; 32], [0u32; 32]);
        reseeding.fill(&mut buf.0);
        reseeding.fill(&mut buf.1);
        let seq = buf;
        for _ in 0..10 {
            reseeding.fill(&mut buf.0);
            reseeding.fill(&mut buf.1);
            assert_eq!(buf, seq);
        }
    }

    #[test]
    fn test_clone_reseeding() {
        let mut zero = StepRng::new(0, 0);
        let rng = ChaCha8Core::from_rng(&mut zero).unwrap();
        let mut rng1 = ReseedingRng::new(rng, 32*4, zero);

        let first: u32 = rng1.gen();
        for _ in 0..10 { let _ = rng1.gen::<u32>(); }

        let mut rng2 = rng1.clone();
        assert_eq!(first, rng2.gen::<u32>());
    }
}

pub mod protocol_message_type;
pub mod shutdown;
pub mod types;

/// Draws the seed of an `ExtranonceAllocator` from the thread-local CSPRNG, which the operating
/// system seeds.
///
/// The allocator derives the `extranonce_prefix` of each channel from its seed, and no client
/// may be able to predict those, so every allocator gets a seed of its own from here.
#[cfg(feature = "std")]
pub fn extranonce_allocator_seed() -> [u8; 16] {
    rand::random()
}

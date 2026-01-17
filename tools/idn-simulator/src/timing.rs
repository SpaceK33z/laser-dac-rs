//! Stream timing utilities for handling IDN timestamps.

use crate::protocol_handler::RenderPoint;

/// A point with an associated stream timestamp.
#[derive(Clone, Debug)]
pub struct TimedPoint {
    /// Timestamp in microseconds (monotonic, unwrapped from u32).
    pub t_us: u64,
    /// The render point data.
    pub p: RenderPoint,
    /// True if this is the first point of a chunk (for debugging).
    pub is_chunk_start: bool,
}

/// State machine for unwrapping u32 timestamps to monotonic u64.
pub struct TimestampUnwrapper {
    last_ts_u32: Option<u32>,
    wrap_base: u64,
}

impl TimestampUnwrapper {
    pub fn new() -> Self {
        Self {
            last_ts_u32: None,
            wrap_base: 0,
        }
    }

    /// Unwrap a u32 timestamp to monotonic u64.
    /// Detects wraps when the new timestamp is significantly smaller than the previous.
    pub fn unwrap(&mut self, ts_u32: u32) -> u64 {
        if let Some(last) = self.last_ts_u32 {
            // If new timestamp is much smaller than last, assume a wrap occurred
            // Use a threshold of half the u32 range to detect wraps
            if ts_u32 < last && (last - ts_u32) > (1u32 << 31) {
                self.wrap_base += 1u64 << 32;
                log::debug!("Timestamp wrap detected: {} -> {}", last, ts_u32);
            }
        }
        self.last_ts_u32 = Some(ts_u32);
        self.wrap_base + ts_u32 as u64
    }

    /// Reset the unwrapper state (e.g., on client disconnect).
    pub fn reset(&mut self) {
        self.last_ts_u32 = None;
        self.wrap_base = 0;
    }
}

impl Default for TimestampUnwrapper {
    fn default() -> Self {
        Self::new()
    }
}

//! Frame adapter for converting point buffers to continuous streams.
//!
//! This module provides [`FrameAdapter`] which converts a point buffer (frame)
//! into a continuous stream of points for the DAC. The adapter cycles through
//! the frame's points, producing chunks on demand.
//!
//! # Update Semantics
//!
//! - **Latest-wins**: `update()` sets the pending frame. Multiple calls before
//!   a swap keep only the most recent.
//! - **Wrap-boundary swaps**: The pending frame becomes current at the start of
//!   the next `next_chunk()` call after the index wraps to 0 (which may happen
//!   mid-chunk). This ensures clean transitions without mid-cycle jumps.
//!
//! The adapter does not insert blanking on frame swaps. If the new frame's first
//! point is far from the previous frame's last point, content should include
//! lead-in blanking to avoid visible travel lines.
//!
//! # Example
//!
//! ```ignore
//! let mut adapter = FrameAdapter::new();
//! adapter.update(Frame::new(points));
//!
//! loop {
//!     let req = stream.next_request()?;
//!     let points = adapter.next_chunk(&req);
//!     stream.write(&req, &points)?;
//! }
//! ```
//!
//! For time-varying animation, use the streaming API directly with a point
//! generator (see the `automatic` example with `orbiting-circle`).

use std::sync::{Arc, Mutex};

use crate::types::{ChunkRequest, LaserPoint};

/// A point buffer to be cycled by the adapter.
#[derive(Clone, Debug)]
pub struct Frame {
    pub points: Vec<LaserPoint>,
}

impl Frame {
    /// Creates a new frame from a vector of points.
    pub fn new(points: Vec<LaserPoint>) -> Self {
        Self { points }
    }

    /// Creates an empty frame (outputs blanked points at last position).
    pub fn empty() -> Self {
        Self { points: Vec::new() }
    }
}

impl From<Vec<LaserPoint>> for Frame {
    fn from(points: Vec<LaserPoint>) -> Self {
        Self::new(points)
    }
}

/// Converts a point buffer (frame) into a continuous stream.
///
/// The adapter cycles through the frame's points, producing exactly
/// `req.n_points` on each `next_chunk()` call.
///
/// # Update semantics
///
/// - `update()` sets the pending frame (latest-wins if called multiple times)
/// - The pending frame becomes current at the start of the next `next_chunk()`
///   after a wrap, ensuring clean transitions without mid-cycle jumps
///
/// # Example
///
/// ```ignore
/// let mut adapter = FrameAdapter::new();
/// adapter.update(Frame::new(circle_points));
///
/// loop {
///     let req = stream.next_request()?;
///     let points = adapter.next_chunk(&req);
///     stream.write(&req, &points)?;
/// }
/// ```
pub struct FrameAdapter {
    current: Frame,
    pending: Option<Frame>,
    point_index: usize,
    /// True when we've wrapped and should apply pending on next chunk.
    swap_pending: bool,
    /// For "hold last" blanking when frame is empty.
    last_position: (f32, f32),
}

impl FrameAdapter {
    /// Creates a new adapter with an empty frame.
    ///
    /// Call `update()` to set the initial frame before streaming.
    pub fn new() -> Self {
        Self {
            current: Frame::empty(),
            pending: None,
            point_index: 0,
            swap_pending: true, // Apply first frame immediately
            last_position: (0.0, 0.0),
        }
    }

    /// Sets the pending frame.
    ///
    /// The frame becomes current at the start of the next `next_chunk()` after
    /// a wrap. If called multiple times before a swap, only the most recent
    /// frame is kept (latest-wins).
    pub fn update(&mut self, frame: Frame) {
        self.pending = Some(frame);
    }

    /// Produces exactly `req.n_points` points.
    ///
    /// Cycles through the current frame, applying any pending frame
    /// only at wrap boundaries.
    pub fn next_chunk(&mut self, req: &ChunkRequest) -> Vec<LaserPoint> {
        if self.swap_pending {
            if let Some(pending) = self.pending.take() {
                self.current = pending;
                self.point_index = 0;
            }
            self.swap_pending = false;
        }

        self.generate_points(req.n_points)
    }

    fn generate_points(&mut self, n_points: usize) -> Vec<LaserPoint> {
        let frame_points = &self.current.points;

        if frame_points.is_empty() {
            let (x, y) = self.last_position;
            return vec![LaserPoint::blanked(x, y); n_points];
        }

        let mut output = Vec::with_capacity(n_points);
        let len = frame_points.len();

        for _ in 0..n_points {
            let point = frame_points[self.point_index];
            output.push(point);
            self.last_position = (point.x, point.y);

            self.point_index += 1;
            if self.point_index >= len {
                self.point_index = 0;
                self.swap_pending = true;
            }
        }

        output
    }

    /// Returns a thread-safe handle for updating frames from another thread.
    pub fn shared(self) -> SharedFrameAdapter {
        SharedFrameAdapter {
            inner: Arc::new(Mutex::new(self)),
        }
    }
}

impl Default for FrameAdapter {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe handle for updating frames from another thread.
#[derive(Clone)]
pub struct SharedFrameAdapter {
    inner: Arc<Mutex<FrameAdapter>>,
}

impl SharedFrameAdapter {
    /// Sets the pending frame. Takes effect at the next wrap boundary.
    pub fn update(&self, frame: Frame) {
        let mut adapter = self.inner.lock().unwrap();
        adapter.update(frame);
    }

    /// Produces exactly `req.n_points` points.
    pub fn next_chunk(&self, req: &ChunkRequest) -> Vec<LaserPoint> {
        let mut adapter = self.inner.lock().unwrap();
        adapter.next_chunk(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::StreamInstant;

    #[test]
    fn test_empty_frame() {
        let mut adapter = FrameAdapter::new();
        let req = ChunkRequest {
            start: StreamInstant(0),
            pps: 30000,
            n_points: 100,
            scheduled_ahead_points: 0,
            device_queued_points: None,
        };

        let points = adapter.next_chunk(&req);
        assert_eq!(points.len(), 100);
        assert!(points.iter().all(|p| p.intensity == 0));
    }

    #[test]
    fn test_frame_cycles() {
        let mut adapter = FrameAdapter::new();
        let frame_points: Vec<LaserPoint> = (0..10)
            .map(|i| LaserPoint::new(i as f32 / 10.0, 0.0, 65535, 0, 0, 65535))
            .collect();
        adapter.update(Frame::new(frame_points));

        let req = ChunkRequest {
            start: StreamInstant(0),
            pps: 30000,
            n_points: 25,
            scheduled_ahead_points: 0,
            device_queued_points: None,
        };

        let points = adapter.next_chunk(&req);
        assert_eq!(points.len(), 25);
    }

    #[test]
    fn test_single_point_swaps_quickly() {
        let mut adapter = FrameAdapter::new();
        adapter.update(Frame::new(vec![LaserPoint::new(
            0.0, 0.0, 65535, 0, 0, 65535,
        )]));

        let req = ChunkRequest {
            start: StreamInstant(0),
            pps: 30000,
            n_points: 10,
            scheduled_ahead_points: 0,
            device_queued_points: None,
        };

        let points1 = adapter.next_chunk(&req);
        assert_eq!(points1[0].x, 0.0);

        adapter.update(Frame::new(vec![LaserPoint::new(
            1.0, 1.0, 0, 65535, 0, 65535,
        )]));

        // Single-point frame wraps every point, so swap happens on next chunk
        let points2 = adapter.next_chunk(&req);
        assert_eq!(points2[0].x, 1.0);
    }

    #[test]
    fn test_swap_waits_for_wrap() {
        let mut adapter = FrameAdapter::new();
        let frame1: Vec<LaserPoint> = (0..100)
            .map(|i| LaserPoint::new(i as f32 / 100.0, 0.0, 65535, 0, 0, 65535))
            .collect();
        adapter.update(Frame::new(frame1));

        let req = ChunkRequest {
            start: StreamInstant(0),
            pps: 30000,
            n_points: 10,
            scheduled_ahead_points: 0,
            device_queued_points: None,
        };

        let points1 = adapter.next_chunk(&req);
        assert_eq!(points1[0].x, 0.0);

        // Update mid-cycle with different frame
        let frame2: Vec<LaserPoint> = (0..100)
            .map(|_| LaserPoint::new(9.0, 9.0, 0, 65535, 0, 65535))
            .collect();
        adapter.update(Frame::new(frame2));

        // Should still use frame1 (not wrapped yet)
        let points2 = adapter.next_chunk(&req);
        assert!(
            (points2[0].x - 0.1).abs() < 1e-4,
            "Expected ~0.1, got {}",
            points2[0].x
        );

        // Output remaining 80 points to complete frame1
        for _ in 0..8 {
            adapter.next_chunk(&req);
        }

        // Now wrapped, uses frame2
        let points_after_wrap = adapter.next_chunk(&req);
        assert_eq!(points_after_wrap[0].x, 9.0);
    }

    #[test]
    fn test_empty_holds_last_position() {
        let mut adapter = FrameAdapter::new();
        adapter.update(Frame::new(vec![LaserPoint::new(
            0.5, -0.3, 65535, 0, 0, 65535,
        )]));

        let req = ChunkRequest {
            start: StreamInstant(0),
            pps: 30000,
            n_points: 5,
            scheduled_ahead_points: 0,
            device_queued_points: None,
        };

        adapter.next_chunk(&req);
        adapter.update(Frame::empty());

        let points = adapter.next_chunk(&req);
        assert!(points.iter().all(|p| p.intensity == 0));
        assert_eq!(points[0].x, 0.5);
        assert_eq!(points[0].y, -0.3);
    }

    #[test]
    fn test_integer_index_deterministic() {
        let mut adapter = FrameAdapter::new();
        let frame: Vec<LaserPoint> = (0..7)
            .map(|i| LaserPoint::new(i as f32, 0.0, 65535, 0, 0, 65535))
            .collect();
        adapter.update(Frame::new(frame));

        let req = ChunkRequest {
            start: StreamInstant(0),
            pps: 30000,
            n_points: 7,
            scheduled_ahead_points: 0,
            device_queued_points: None,
        };

        // No drift over 1000 cycles
        for cycle in 0..1000 {
            let points = adapter.next_chunk(&req);
            for (i, p) in points.iter().enumerate() {
                assert_eq!(p.x, i as f32, "Cycle {}: drift detected", cycle);
            }
        }
    }

    #[test]
    fn test_from_vec() {
        let points: Vec<LaserPoint> = vec![LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535)];
        let frame: Frame = points.into();
        assert_eq!(frame.points.len(), 1);
    }
}

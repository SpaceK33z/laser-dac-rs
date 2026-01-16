//! Frame adapters for converting frame-based sources to streams.
//!
//! Frames remain supported as a convenience, but they feed a stream rather than
//! being written to DACs directly. The `FrameAdapter` converts frames to chunks
//! matching each `ChunkRequest`.

use std::sync::{Arc, Mutex};

use crate::types::{ChunkRequest, LaserPoint, StreamInstant};

/// A frame of laser points with a conceptual frame rate.
///
/// Note: The adapter uses "loop-complete" semantics where a frame lasts exactly
/// as many points as it contains. The `fps` field is reserved for future use.
#[derive(Clone, Debug)]
pub struct Frame {
    pub points: Vec<LaserPoint>,
    /// Reserved for future frame-rate-aware switching; currently unused.
    pub fps: f32,
}

impl Frame {
    pub fn new(points: Vec<LaserPoint>, fps: f32) -> Self {
        Self { points, fps }
    }

    pub fn blank(fps: f32) -> Self {
        Self {
            points: Vec::new(),
            fps,
        }
    }
}

/// Trait for sources that produce frames on demand.
///
/// The adapter uses "loop-complete" semantics: a new frame is requested only
/// after all points in the current frame have been output.
pub trait FrameSource: Send {
    /// Produces the next frame for the given stream time and PPS.
    fn next_frame(&mut self, t: StreamInstant, pps: u32) -> Frame;
}

/// Converts frames to chunks matching each `ChunkRequest`.
///
/// Supports two modes:
/// - **Latest frame mode**: Push frames via `update_frame()`, pull chunks via `next_chunk()`.
/// - **Pull-based mode**: Adapter pulls frames from a `FrameSource`.
///
/// # Frame switching behavior
///
/// Both modes use wrap-driven switching:
/// - The current frame loops continuously, outputting points in chunks
/// - When the index wraps back to 0, the adapter marks itself ready to switch
/// - On the *next* `next_chunk()` call, the switch occurs (never mid-chunk)
///
/// # Example
///
/// ```ignore
/// let mut adapter = FrameAdapter::latest(30.0);
/// adapter.update_frame(Frame::new(points, 30.0));
///
/// let req = stream.next_request()?;
/// let points = adapter.next_chunk(&req);
/// stream.write(&req, &points)?;
/// ```
pub struct FrameAdapter {
    inner: FrameAdapterInner,
}

enum FrameAdapterInner {
    Latest(LatestFrameAdapter),
    Source(SourceFrameAdapter),
}

struct LatestFrameAdapter {
    current_frame: Frame,
    pending_frame: Option<Frame>,
    point_index: usize,
    /// Set when index wraps; triggers frame swap on next chunk.
    swap_pending: bool,
    /// For "hold last" blanking when frame is empty.
    last_position: (f32, f32),
    #[allow(dead_code)]
    fps: f32,
}

struct SourceFrameAdapter {
    source: Box<dyn FrameSource>,
    current_frame: Option<Frame>,
    point_index: usize,
    /// Set on wrap or initially; triggers fetch on next chunk.
    need_new_frame: bool,
    last_position: (f32, f32),
}

impl FrameAdapter {
    /// Creates a new adapter in "latest frame" mode.
    ///
    /// Push frames via `update_frame()`. The adapter uses the most recent frame,
    /// switching only after completing a full scan of the current frame.
    pub fn latest(fps: f32) -> Self {
        Self {
            inner: FrameAdapterInner::Latest(LatestFrameAdapter {
                current_frame: Frame::blank(fps),
                pending_frame: None,
                point_index: 0,
                swap_pending: true, // Apply first frame immediately
                last_position: (0.0, 0.0),
                fps,
            }),
        }
    }

    /// Creates a new adapter in pull-based mode.
    ///
    /// The adapter calls `source.next_frame()` after completing each frame.
    pub fn from_source(source: Box<dyn FrameSource>) -> Self {
        Self {
            inner: FrameAdapterInner::Source(SourceFrameAdapter {
                source,
                current_frame: None,
                point_index: 0,
                need_new_frame: true,
                last_position: (0.0, 0.0),
            }),
        }
    }

    /// Updates the frame (latest-frame mode only).
    ///
    /// The new frame becomes current after the current frame completes a full loop.
    /// Multiple updates before a wrap keep only the most recent frame.
    ///
    /// # Panics
    ///
    /// Panics if called on a source-based adapter.
    pub fn update_frame(&mut self, frame: Frame) {
        match &mut self.inner {
            FrameAdapterInner::Latest(adapter) => {
                adapter.pending_frame = Some(frame);
            }
            FrameAdapterInner::Source(_) => {
                panic!("update_frame() called on source-based adapter");
            }
        }
    }

    /// Produces exactly `req.n_points` points.
    ///
    /// Cycles through the current frame, switching frames only at wrap boundaries.
    pub fn next_chunk(&mut self, req: &ChunkRequest) -> Vec<LaserPoint> {
        match &mut self.inner {
            FrameAdapterInner::Latest(adapter) => adapter.next_chunk(req),
            FrameAdapterInner::Source(adapter) => adapter.next_chunk(req),
        }
    }

    /// Returns a thread-safe handle for updating frames from another thread.
    ///
    /// # Panics
    ///
    /// Panics if called on a source-based adapter.
    pub fn shared(self) -> SharedFrameAdapter {
        match self.inner {
            FrameAdapterInner::Latest(adapter) => SharedFrameAdapter {
                inner: Arc::new(Mutex::new(adapter)),
            },
            FrameAdapterInner::Source(_) => {
                panic!("shared() called on source-based adapter");
            }
        }
    }
}

impl LatestFrameAdapter {
    fn next_chunk(&mut self, req: &ChunkRequest) -> Vec<LaserPoint> {
        if self.swap_pending {
            if let Some(pending) = self.pending_frame.take() {
                self.current_frame = pending;
                self.point_index = 0;
            }
            self.swap_pending = false;
        }
        self.generate_points(req.n_points)
    }

    fn generate_points(&mut self, n_points: usize) -> Vec<LaserPoint> {
        let frame_points = &self.current_frame.points;

        if frame_points.is_empty() {
            let (x, y) = self.last_position;
            return vec![LaserPoint::blanked(x, y); n_points];
        }

        let mut output = Vec::with_capacity(n_points);
        let frame_len = frame_points.len();

        for _ in 0..n_points {
            let point = frame_points[self.point_index];
            output.push(point);
            self.last_position = (point.x, point.y);

            self.point_index += 1;
            if self.point_index >= frame_len {
                self.point_index = 0;
                self.swap_pending = true;
            }
        }

        output
    }
}

impl SourceFrameAdapter {
    fn next_chunk(&mut self, req: &ChunkRequest) -> Vec<LaserPoint> {
        if self.need_new_frame {
            let frame = self.source.next_frame(req.start, req.pps);
            self.current_frame = Some(frame);
            self.point_index = 0;
            self.need_new_frame = false;
        }
        self.generate_points(req.n_points)
    }

    fn generate_points(&mut self, n_points: usize) -> Vec<LaserPoint> {
        let Some(ref frame) = self.current_frame else {
            let (x, y) = self.last_position;
            return vec![LaserPoint::blanked(x, y); n_points];
        };

        let frame_points = &frame.points;

        if frame_points.is_empty() {
            let (x, y) = self.last_position;
            self.need_new_frame = true;
            return vec![LaserPoint::blanked(x, y); n_points];
        }

        let mut output = Vec::with_capacity(n_points);
        let frame_len = frame_points.len();

        for _ in 0..n_points {
            let point = frame_points[self.point_index];
            output.push(point);
            self.last_position = (point.x, point.y);

            self.point_index += 1;
            if self.point_index >= frame_len {
                self.point_index = 0;
                self.need_new_frame = true;
            }
        }

        output
    }
}

/// Thread-safe handle for updating frames from another thread.
#[derive(Clone)]
pub struct SharedFrameAdapter {
    inner: Arc<Mutex<LatestFrameAdapter>>,
}

impl SharedFrameAdapter {
    /// Updates the frame. Takes effect after the current frame completes.
    pub fn update_frame(&self, frame: Frame) {
        let mut adapter = self.inner.lock().unwrap();
        adapter.pending_frame = Some(frame);
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

    #[test]
    fn test_latest_frame_empty() {
        let mut adapter = FrameAdapter::latest(30.0);
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
    fn test_latest_frame_cycles() {
        let mut adapter = FrameAdapter::latest(30.0);
        let frame_points: Vec<LaserPoint> = (0..10)
            .map(|i| LaserPoint::new(i as f32 / 10.0, 0.0, 65535, 0, 0, 65535))
            .collect();
        adapter.update_frame(Frame::new(frame_points, 30.0));

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
    fn test_single_point_frame_swaps_quickly() {
        let mut adapter = FrameAdapter::latest(30.0);
        adapter.update_frame(Frame::new(
            vec![LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535)],
            30.0,
        ));

        let req = ChunkRequest {
            start: StreamInstant(0),
            pps: 30000,
            n_points: 10,
            scheduled_ahead_points: 0,
            device_queued_points: None,
        };

        let points1 = adapter.next_chunk(&req);
        assert_eq!(points1[0].x, 0.0);

        adapter.update_frame(Frame::new(
            vec![LaserPoint::new(1.0, 1.0, 0, 65535, 0, 65535)],
            30.0,
        ));

        // Single-point frame wraps every point, so swap happens on next chunk
        let points2 = adapter.next_chunk(&req);
        assert_eq!(points2[0].x, 1.0);
    }

    #[test]
    fn test_frame_swap_waits_for_wrap() {
        let mut adapter = FrameAdapter::latest(30.0);
        let frame1: Vec<LaserPoint> = (0..100)
            .map(|i| LaserPoint::new(i as f32 / 100.0, 0.0, 65535, 0, 0, 65535))
            .collect();
        adapter.update_frame(Frame::new(frame1, 30.0));

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
        adapter.update_frame(Frame::new(frame2, 30.0));

        // Should still use frame1 (not wrapped yet)
        let points2 = adapter.next_chunk(&req);
        assert!((points2[0].x - 0.1).abs() < 1e-4, "Expected ~0.1, got {}", points2[0].x);

        // Output remaining 80 points to complete frame1
        for _ in 0..8 {
            adapter.next_chunk(&req);
        }

        // Now wrapped, uses frame2
        let points_after_wrap = adapter.next_chunk(&req);
        assert_eq!(points_after_wrap[0].x, 9.0);
    }

    #[test]
    fn test_empty_frame_holds_last_position() {
        let mut adapter = FrameAdapter::latest(30.0);
        adapter.update_frame(Frame::new(
            vec![LaserPoint::new(0.5, -0.3, 65535, 0, 0, 65535)],
            30.0,
        ));

        let req = ChunkRequest {
            start: StreamInstant(0),
            pps: 30000,
            n_points: 5,
            scheduled_ahead_points: 0,
            device_queued_points: None,
        };

        adapter.next_chunk(&req);
        adapter.update_frame(Frame::blank(30.0));

        let points = adapter.next_chunk(&req);
        assert!(points.iter().all(|p| p.intensity == 0));
        assert_eq!(points[0].x, 0.5);
        assert_eq!(points[0].y, -0.3);
    }

    #[test]
    fn test_integer_index_deterministic() {
        let mut adapter = FrameAdapter::latest(30.0);
        let frame: Vec<LaserPoint> = (0..7)
            .map(|i| LaserPoint::new(i as f32, 0.0, 65535, 0, 0, 65535))
            .collect();
        adapter.update_frame(Frame::new(frame, 30.0));

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
    fn test_source_adapter_wrap_driven_fetch() {
        struct CountingSource {
            frame_count: usize,
        }

        impl FrameSource for CountingSource {
            fn next_frame(&mut self, _t: StreamInstant, _pps: u32) -> Frame {
                self.frame_count += 1;
                let x = self.frame_count as f32;
                Frame::new(
                    (0..10).map(|_| LaserPoint::new(x, 0.0, 65535, 0, 0, 65535)).collect(),
                    30.0,
                )
            }
        }

        let mut adapter = FrameAdapter::from_source(Box::new(CountingSource { frame_count: 0 }));

        let req = ChunkRequest {
            start: StreamInstant(0),
            pps: 30000,
            n_points: 5,
            scheduled_ahead_points: 0,
            device_queued_points: None,
        };

        // First chunk: fetches frame 1
        assert_eq!(adapter.next_chunk(&req)[0].x, 1.0);
        // Second chunk: still frame 1
        assert_eq!(adapter.next_chunk(&req)[0].x, 1.0);
        // Third chunk: wrapped, fetches frame 2
        assert_eq!(adapter.next_chunk(&req)[0].x, 2.0);
    }
}

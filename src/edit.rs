//! Slices of the recording that make the final video, and the mapping between output and source time.
use serde::{Deserialize, Serialize};

pub const MIN_SLICE: f64 = 0.1;
pub const SPEEDS: [f64; 6] = [0.5, 1.0, 1.5, 2.0, 3.0, 4.0];

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Slice {
    pub start: f64,
    pub end: f64,
    pub speed: f64,
}

impl Slice {
    pub fn len_out(&self) -> f64 {
        (self.end - self.start).max(0.0) / self.speed
    }
}

/// Ordered, non-overlapping slices in source time. Gaps between them are cut from the output.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Timeline {
    pub slices: Vec<Slice>,
}

impl Timeline {
    pub fn full(duration: f64) -> Self {
        Self { slices: vec![Slice { start: 0.0, end: duration, speed: 1.0 }] }
    }

    pub fn is_identity(&self, duration: f64) -> bool {
        matches!(self.slices.as_slice(), [s] if s.start <= 1e-6 && s.end >= duration - 1e-6 && s.speed == 1.0)
    }

    /// Source time of the last frame in the output.
    pub fn source_end(&self, duration: f64) -> f64 {
        self.slices.last().map_or(duration, |s| s.end.min(duration))
    }

    pub fn duration(&self) -> f64 {
        self.slices.iter().map(Slice::len_out).sum()
    }

    /// Output start and end of slice `i`.
    pub fn bounds(&self, i: usize) -> (f64, f64) {
        let start: f64 = self.slices[..i].iter().map(Slice::len_out).sum();
        (start, start + self.slices[i].len_out())
    }

    pub fn index_at(&self, t: f64) -> usize {
        let mut acc = 0.0;
        for (i, s) in self.slices.iter().enumerate() {
            acc += s.len_out();
            if t < acc {
                return i;
            }
        }
        self.slices.len().saturating_sub(1)
    }

    pub fn to_source(&self, t: f64) -> f64 {
        let Some(last) = self.slices.last() else { return t };
        let mut acc = 0.0;
        for s in &self.slices {
            let len = s.len_out();
            if t < acc + len {
                return s.start + (t - acc).max(0.0) * s.speed;
            }
            acc += len;
        }
        last.end
    }

    /// Never decreases; source times inside a cut map to where the cut sits in the output.
    pub fn to_output(&self, src: f64) -> f64 {
        let mut acc = 0.0;
        for s in &self.slices {
            if src < s.start {
                return acc;
            }
            if src <= s.end {
                return acc + (src - s.start) / s.speed;
            }
            acc += s.len_out();
        }
        acc
    }

    pub fn contains_source(&self, src: f64) -> bool {
        self.slices.iter().any(|s| src >= s.start && src <= s.end)
    }

    /// Splits the slice under output time `t`. Returns the index of the right half.
    pub fn split(&mut self, t: f64) -> Option<usize> {
        let i = self.index_at(t);
        let src = self.to_source(t);
        let s = *self.slices.get(i)?;
        if src - s.start < MIN_SLICE || s.end - src < MIN_SLICE {
            return None;
        }
        self.slices[i].end = src;
        self.slices.insert(i + 1, Slice { start: src, ..s });
        Some(i + 1)
    }

    pub fn remove(&mut self, i: usize) -> bool {
        if self.slices.len() <= 1 || i >= self.slices.len() {
            return false;
        }
        self.slices.remove(i);
        true
    }

    /// Room a slice edge may move within, in source time.
    pub fn edge_limits(&self, i: usize, duration: f64) -> (f64, f64) {
        let lo = if i == 0 { 0.0 } else { self.slices[i - 1].end };
        let hi = self.slices.get(i + 1).map_or(duration, |s| s.start);
        (lo, hi)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_through_cuts_and_speed() {
        let tl = Timeline {
            slices: vec![Slice { start: 0.0, end: 2.0, speed: 1.0 }, Slice { start: 4.0, end: 8.0, speed: 2.0 }],
        };
        assert_eq!(tl.duration(), 4.0);
        assert_eq!(tl.to_source(1.0), 1.0);
        assert_eq!(tl.to_source(3.0), 6.0);
        assert_eq!(tl.to_output(3.0), 2.0);
        assert_eq!(tl.to_output(6.0), 3.0);
        assert_eq!(tl.to_source(10.0), 8.0);
        assert_eq!(tl.index_at(2.5), 1);
    }

    #[test]
    fn split_and_remove() {
        let mut tl = Timeline::full(10.0);
        assert_eq!(tl.split(4.0), Some(1));
        assert_eq!(tl.slices.len(), 2);
        assert!(tl.remove(0));
        assert_eq!(tl.duration(), 6.0);
        assert_eq!(tl.to_source(0.0), 4.0);
        assert!(!tl.remove(0));
    }
}

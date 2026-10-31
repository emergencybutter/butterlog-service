//! The channel table a session opens with (design §3.1).

/// What a channel carries. Every kind but `String` travels as a quantised integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    F64,
    I64,
    Bool,
    Enum,
    String,
}

/// How often the sampler reads a channel (design §3.3); the rates per class are tunables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateClass {
    Frame,
    Fast,
    Slow,
    OnChange,
}

/// How a channel's values are differenced within a chunk. Smoothly varying channels
/// (position, attitude) compress best as delta-of-delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaOrder {
    Delta,
    DeltaOfDelta,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Channel {
    pub id: u16,
    /// SimVar or dataref path, plus index for arrays.
    pub name: String,
    /// Sim-native unit.
    pub unit: String,
    pub kind: Kind,
    /// Quantisation step, e.g. 1e-7 for lat/lon, 0.1 for feet. Ignored for `String`.
    pub quantum: f64,
    pub rate_class: RateClass,
    pub delta: DeltaOrder,
}

impl Channel {
    /// Quantises a sim value onto this channel's integer grid.
    pub fn quantise(&self, v: f64) -> i64 {
        (v / self.quantum).round() as i64
    }

    /// The value a quantised integer stands for.
    pub fn dequantise(&self, q: i64) -> f64 {
        q as f64 * self.quantum
    }
}

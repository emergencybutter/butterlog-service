//! Quantise-then-compare: the deadband that makes change-only ticks pay off (design §3.2).

use crate::chunk::Value;
use std::collections::HashMap;

/// Remembers the last value sent per channel and lets through only values that differ.
/// Runs in the sampler, on values already quantised with [`crate::Channel::quantise`].
#[derive(Default)]
pub struct ChangeFilter {
    last: HashMap<u16, Value>,
}

impl ChangeFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the value if it differs from the last one let through for this channel.
    pub fn filter(&mut self, channel: u16, value: Value) -> Option<Value> {
        if self.last.get(&channel) == Some(&value) {
            return None;
        }
        self.last.insert(channel, value.clone());
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_repeats_only() {
        let mut f = ChangeFilter::new();
        assert_eq!(f.filter(1, Value::Int(5)), Some(Value::Int(5)));
        assert_eq!(f.filter(1, Value::Int(5)), None);
        assert_eq!(f.filter(2, Value::Int(5)), Some(Value::Int(5)));
        assert_eq!(f.filter(1, Value::Int(6)), Some(Value::Int(6)));
    }
}

//! Per-phase sampling rates (design §3.3) and the change filter (§3.2), shared by every
//! capture path: the app's SimConnect sampler, the X-Plane plugin and the offline
//! `measure_capture` replay.
//!
//! The caller sets each channel's latest raw value as the sim reports it, then calls
//! [`Sampler::tick`] once per sim frame. A tick carries the channels whose rate class is due
//! in the current flight phase and whose quantised value changed.

use crate::chunk::{Tick, Value};
use crate::quantise::ChangeFilter;
use crate::schema::{Channel, RateClass};
use std::collections::BTreeMap;

/// The channels the phase test reads, in the sim's own units, and its thresholds.
#[derive(Debug, Clone)]
pub struct PhaseInputs {
    pub agl: u16,
    pub on_ground: u16,
    pub vertical_speed: u16,
    /// Bank in degrees, in both sims.
    pub bank: u16,
    /// "Low": below this height above ground (~1,000 ft).
    pub low_agl: f64,
    /// "Manoeuvre": vertical speed beyond this (~500 fpm) or banked beyond 5°.
    pub steady_vs: f64,
}

impl PhaseInputs {
    /// For the SimConnect variables `msfs_capture` and the app request, in feet and fpm.
    pub fn msfs(agl: u16, on_ground: u16, vertical_speed: u16, bank: u16) -> Self {
        Self { agl, on_ground, vertical_speed, bank, low_agl: 1000.0, steady_vs: 500.0 }
    }

    /// For the X-Plane datarefs `y_agl`, `onground_any`, `vh_ind` and `phi`, in m and m/s.
    pub fn xplane(agl: u16, on_ground: u16, vertical_speed: u16, bank: u16) -> Self {
        Self { agl, on_ground, vertical_speed, bank, low_agl: 305.0, steady_vs: 2.5 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// On the ground or below ~1,000 ft AGL.
    Low,
    /// Climbing, descending or banked.
    Manoeuvre,
    Cruise,
}

const BANKED_DEG: f64 = 5.0;

/// Seconds between samples of a rate class in a phase; 0 means every frame.
pub fn period(class: RateClass, phase: Phase, low_hz: f64) -> f64 {
    match (class, phase) {
        (RateClass::OnChange, _) => 0.0,
        (RateClass::Frame, Phase::Low) => 1.0 / low_hz,
        (RateClass::Frame, Phase::Manoeuvre) | (RateClass::Fast, _) => 0.2,
        (RateClass::Frame, Phase::Cruise) => 1.0,
        (RateClass::Slow, Phase::Cruise) => 5.0,
        (RateClass::Slow, _) => 1.0,
    }
}

/// Every frame near the ground, capped at this rate (design §3.3).
pub const DEFAULT_LOW_HZ: f64 = 30.0;

const CLASSES: [RateClass; 4] =
    [RateClass::Frame, RateClass::Fast, RateClass::Slow, RateClass::OnChange];

fn class_index(class: RateClass) -> usize {
    CLASSES.iter().position(|c| *c == class).unwrap()
}

pub struct Sampler {
    channels: BTreeMap<u16, Channel>,
    phase_inputs: PhaseInputs,
    low_hz: f64,
    latest: BTreeMap<u16, f64>,
    filter: ChangeFilter,
    /// Sim time each rate class was last sampled at.
    last_sample: [Option<f64>; 4],
    last_sim_time: Option<f64>,
    last_flags: Option<u8>,
}

impl Sampler {
    pub fn new(channels: &[Channel], phase_inputs: PhaseInputs) -> Self {
        Self {
            channels: channels.iter().map(|c| (c.id, c.clone())).collect(),
            phase_inputs,
            low_hz: DEFAULT_LOW_HZ,
            latest: BTreeMap::new(),
            filter: ChangeFilter::new(),
            last_sample: [None; 4],
            last_sim_time: None,
            last_flags: None,
        }
    }

    pub fn with_low_hz(mut self, low_hz: f64) -> Self {
        self.low_hz = low_hz;
        self
    }

    /// Forgets what was sent, keeping the latest values, so the next tick carries every
    /// channel: for a new session over a source that reports only changes (SimConnect
    /// `CHANGED`), whose unchanged values would otherwise never reach it.
    pub fn restart(&mut self) {
        self.filter = ChangeFilter::new();
        self.last_sample = [None; 4];
        self.last_flags = None;
    }

    /// Records a channel's latest value as the sim reports it. Unknown channels and values
    /// that aren't finite are ignored.
    pub fn set(&mut self, channel: u16, value: f64) {
        if self.channels.contains_key(&channel) && value.is_finite() {
            self.latest.insert(channel, value);
        }
    }

    /// The flight phase from the latest values. Low until the phase inputs are known.
    pub fn phase(&self) -> Phase {
        let p = &self.phase_inputs;
        let v = |id| self.latest.get(&id).copied();
        let (Some(agl), Some(on_ground)) = (v(p.agl), v(p.on_ground)) else {
            return Phase::Low;
        };
        if on_ground != 0.0 || agl < p.low_agl {
            Phase::Low
        } else if v(p.vertical_speed).is_some_and(|vs| vs.abs() > p.steady_vs)
            || v(p.bank).is_some_and(|b| b.abs() > BANKED_DEG)
        {
            Phase::Manoeuvre
        } else {
            Phase::Cruise
        }
    }

    /// Samples the latest values at `sim_time`. Returns `None` when nothing changed and the
    /// state flags are the same as in the last tick.
    ///
    /// Sim time going backwards (a flight loaded, MSFS resetting its clock) restarts every
    /// rate class, so the next ticks aren't held back by times from before the jump.
    pub fn tick(&mut self, sim_time: f64, wall_time_ms: i64, flags: u8) -> Option<Tick> {
        if self.last_sim_time.is_some_and(|last| sim_time < last) {
            self.last_sample = [None; 4];
        }
        self.last_sim_time = Some(sim_time);
        let phase = self.phase();
        let mut due = [false; 4];
        for (i, class) in CLASSES.iter().enumerate() {
            let p = period(*class, phase, self.low_hz);
            // Within 1 ms of the period counts as due, so 30 Hz doesn't alias to 20 at 60 fps.
            if self.last_sample[i].is_none_or(|last| sim_time - last >= p - 0.001) {
                due[i] = true;
                self.last_sample[i] = Some(sim_time);
            }
        }
        let mut values = Vec::new();
        for (id, raw) in &self.latest {
            let channel = &self.channels[id];
            if !due[class_index(channel.rate_class)] {
                continue;
            }
            if let Some(v) = self.filter.filter(*id, Value::Int(channel.quantise(*raw))) {
                values.push((*id, v));
            }
        }
        if values.is_empty() && self.last_flags == Some(flags) {
            return None;
        }
        self.last_flags = Some(flags);
        Some(Tick { sim_time, wall_time_ms, flags, values })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{DeltaOrder, Kind};

    const AGL: u16 = 0;
    const GROUND: u16 = 1;
    const VS: u16 = 2;
    const BANK: u16 = 3;
    const RPM: u16 = 4;
    const LIGHT: u16 = 5;
    const LAT: u16 = 6;

    fn channel(id: u16, rate_class: RateClass, quantum: f64) -> Channel {
        Channel {
            id,
            name: format!("c{id}"),
            unit: String::new(),
            kind: Kind::F64,
            quantum,
            rate_class,
            delta: DeltaOrder::Delta,
        }
    }

    fn sampler() -> Sampler {
        let channels = [
            channel(AGL, RateClass::Frame, 0.1),
            channel(GROUND, RateClass::OnChange, 1.0),
            channel(VS, RateClass::Frame, 1.0),
            channel(BANK, RateClass::Frame, 0.01),
            channel(RPM, RateClass::Slow, 1.0),
            channel(LIGHT, RateClass::OnChange, 1.0),
            channel(LAT, RateClass::Frame, 1e-7),
        ];
        Sampler::new(&channels, PhaseInputs::msfs(AGL, GROUND, VS, BANK))
    }

    fn ids(tick: &Tick) -> Vec<u16> {
        tick.values.iter().map(|(id, _)| *id).collect()
    }

    /// Runs 60 fps frames for `secs`, changing every channel each frame, and counts how
    /// often each channel was sent.
    fn rates(s: &mut Sampler, from: f64, secs: f64) -> BTreeMap<u16, usize> {
        let mut sent = BTreeMap::new();
        let frames = (secs * 60.0) as usize;
        for f in 0..frames {
            let t = from + f as f64 / 60.0;
            s.set(RPM, 2000.0 + f as f64);
            s.set(LAT, 46.0 + t * 1e-5);
            s.set(LIGHT, (f % 2) as f64);
            if let Some(tick) = s.tick(t, (t * 1000.0) as i64, 0) {
                for id in ids(&tick) {
                    *sent.entry(id).or_default() += 1;
                }
            }
        }
        sent
    }

    #[test]
    fn rates_follow_the_phase() {
        let mut s = sampler();
        s.set(AGL, 0.0);
        s.set(GROUND, 1.0);
        s.set(VS, 0.0);
        assert_eq!(s.phase(), Phase::Low);
        let low = rates(&mut s, 0.0, 10.0);
        assert_eq!(low[&LAT], 300, "30 Hz near the ground");
        assert_eq!(low[&RPM], 10, "1 Hz");
        assert_eq!(low[&LIGHT], 600, "every frame it changes");

        s.set(AGL, 5000.0);
        s.set(GROUND, 0.0);
        s.set(VS, 0.0);
        assert_eq!(s.phase(), Phase::Cruise);
        let cruise = rates(&mut s, 10.0, 10.0);
        assert_eq!(cruise[&LAT], 10, "1 Hz in cruise");
        assert_eq!(cruise[&RPM], 2, "0.2 Hz in cruise");

        s.set(VS, -800.0);
        assert_eq!(s.phase(), Phase::Manoeuvre);
        let manoeuvre = rates(&mut s, 20.0, 10.0);
        assert_eq!(manoeuvre[&LAT], 50, "5 Hz manoeuvring");
    }

    #[test]
    fn sends_only_quantised_changes() {
        let mut s = sampler();
        s.set(AGL, 10.0);
        s.set(GROUND, 1.0);
        let first = s.tick(0.0, 0, 0).unwrap();
        assert_eq!(ids(&first), vec![AGL, GROUND]);
        // Below half a quantum: the same quantised value, and nothing else changed.
        s.set(AGL, 10.04);
        assert_eq!(s.tick(1.0, 1000, 0), None);
        s.set(AGL, 10.06);
        assert_eq!(s.tick(2.0, 2000, 0).unwrap().values, vec![(AGL, Value::Int(101))]);
    }

    #[test]
    fn a_change_of_state_alone_is_a_tick() {
        let mut s = sampler();
        s.set(GROUND, 1.0);
        s.tick(0.0, 0, 0).unwrap();
        assert_eq!(s.tick(1.0, 1000, 0), None);
        let paused = s.tick(1.0, 1500, crate::flags::PAUSED).unwrap();
        assert!(paused.values.is_empty());
        assert_eq!(s.tick(1.0, 2000, crate::flags::PAUSED), None);
    }

    #[test]
    fn sim_time_going_backwards_restarts_the_rates() {
        let mut s = sampler();
        s.set(AGL, 5000.0);
        s.set(GROUND, 0.0);
        s.set(RPM, 2000.0);
        s.tick(100.0, 0, 0).unwrap();
        // Cruise: RPM next due at 105 s. After the clock resets to 0, it's due at once.
        s.set(RPM, 2100.0);
        assert_eq!(ids(&s.tick(0.0, 1000, 0).unwrap()), vec![RPM]);
    }

    #[test]
    fn a_restart_sends_every_latest_value_again() {
        let mut s = sampler();
        s.set(AGL, 10.0);
        s.set(GROUND, 1.0);
        s.tick(0.0, 0, 0).unwrap();
        assert_eq!(s.tick(1.0, 1000, 0), None);
        s.restart();
        assert_eq!(ids(&s.tick(1.0, 1500, 0).unwrap()), vec![AGL, GROUND]);
    }

    #[test]
    fn ignores_unknown_channels_and_non_finite_values() {
        let mut s = sampler();
        s.set(99, 1.0);
        s.set(AGL, f64::NAN);
        assert_eq!(s.tick(0.0, 0, 0).map(|t| t.values), Some(vec![]));
    }
}

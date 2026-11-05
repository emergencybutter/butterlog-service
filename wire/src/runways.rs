//! The runways of an airport as the sim draws them, sent by the app in a
//! [`EventKind::Runways`](crate::EventKind::Runways) event at touchdown: where the aircraft
//! touched down is measured against these, not against a public database whose
//! thresholds can be tens of feet from the sim's scenery.
//!
//! The event's detail is text, one line per runway after the airport's ident:
//!
//! ```text
//! PHJH
//! 100 3000 02 20.9592990 -156.6750030 28.00 0 20 20.9666002 -156.6710052 208.00 0
//! ```
//!
//! width and length in feet, then each end: ident, threshold latitude and longitude
//! (degrees), true heading (degrees) and displaced threshold (feet).

/// One end of a runway: where its threshold is and which way it points.
#[derive(Debug, Clone, PartialEq)]
pub struct RunwayEnd {
    /// e.g. "26L".
    pub ident: String,
    pub latitude: f64,
    pub longitude: f64,
    /// True, degrees.
    pub heading: f64,
    pub displaced_ft: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SimRunway {
    pub width_ft: f64,
    pub length_ft: f64,
    pub ends: [RunwayEnd; 2],
}

/// An airport's runways, as the sim has them.
#[derive(Debug, Clone, PartialEq)]
pub struct SimRunways {
    pub airport: String,
    pub runways: Vec<SimRunway>,
}

impl SimRunways {
    pub fn encode(&self) -> String {
        let mut out = self.airport.replace(char::is_whitespace, "");
        for r in &self.runways {
            out.push_str(&format!("\n{:.0} {:.0}", r.width_ft, r.length_ft));
            for e in &r.ends {
                let ident = if e.ident.trim().is_empty() { "-".to_string() } else { e.ident.replace(char::is_whitespace, "") };
                out.push_str(&format!(
                    " {ident} {:.7} {:.7} {:.2} {:.0}",
                    e.latitude, e.longitude, e.heading, e.displaced_ft
                ));
            }
        }
        out
    }

    /// `None` if the text is not runways; a line that does not parse is skipped.
    pub fn decode(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        let airport = lines.next()?.trim().to_string();
        let runways = lines.filter_map(decode_runway).collect();
        Some(Self { airport, runways })
    }
}

fn decode_runway(line: &str) -> Option<SimRunway> {
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() != 12 {
        return None;
    }
    let num = |i: usize| f[i].parse::<f64>().ok().filter(|v| v.is_finite());
    let end = |at: usize| -> Option<RunwayEnd> {
        Some(RunwayEnd {
            ident: f[at].to_string(),
            latitude: num(at + 1)?,
            longitude: num(at + 2)?,
            heading: num(at + 3)?,
            displaced_ft: num(at + 4)?,
        })
    };
    Some(SimRunway { width_ft: num(0)?, length_ft: num(1)?, ends: [end(2)?, end(7)?] })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_skips_what_does_not_parse() {
        let runways = SimRunways {
            airport: "PHJH".into(),
            runways: vec![SimRunway {
                width_ft: 100.0,
                length_ft: 3000.0,
                ends: [
                    RunwayEnd { ident: "02".into(), latitude: 20.959299, longitude: -156.675003, heading: 28.0, displaced_ft: 0.0 },
                    RunwayEnd { ident: "20".into(), latitude: 20.9666, longitude: -156.671005, heading: 208.0, displaced_ft: 150.0 },
                ],
            }],
        };
        let text = runways.encode();
        assert_eq!(SimRunways::decode(&text), Some(runways.clone()));
        let with_junk = format!("{text}\nnot a runway\n1 2 3");
        assert_eq!(SimRunways::decode(&with_junk), Some(runways));
        assert_eq!(SimRunways::decode(""), None);
    }
}

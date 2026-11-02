//! Loader for `airlines.csv` (`icao,airline,callsign`) — the airline reference table
//! used to identify an operator from a free string (sim title / livery).

use std::error::Error;
use std::sync::OnceLock;

/// The table, built into the crate so the app and the service read the same one.
pub const CSV: &str = include_str!("../data/airlines.csv");

#[derive(Debug, Clone)]
pub struct Airline {
    pub icao: String,
    pub name: String,
    pub callsign: String,
}

pub struct AirlinesDatabase {
    pub airlines: Vec<Airline>,
}

impl AirlinesDatabase {
    /// The built-in table, parsed once.
    pub fn embedded() -> &'static Self {
        static DB: OnceLock<AirlinesDatabase> = OnceLock::new();
        DB.get_or_init(|| Self::parse(CSV).expect("the built-in airline table parses"))
    }

    pub fn parse(csv: &str) -> Result<Self, Box<dyn Error>> {
        let mut reader = csv::Reader::from_reader(csv.as_bytes());
        let headers = reader.headers()?.clone();

        let icao_idx = headers.iter().position(|h| h == "icao");
        let name_idx = headers.iter().position(|h| h == "airline");
        let callsign_idx = headers.iter().position(|h| h == "callsign");

        let mut airlines = Vec::new();
        if let (Some(ii), Some(ni), Some(ci)) = (icao_idx, name_idx, callsign_idx) {
            for record in reader.records().flatten() {
                let icao = record.get(ii).unwrap_or("").trim().to_uppercase();
                let name = record.get(ni).unwrap_or("").trim().to_string();
                let callsign = record.get(ci).unwrap_or("").trim().to_string();
                if !icao.is_empty() && !name.is_empty() {
                    airlines.push(Airline { icao, name, callsign });
                }
            }
        }
        Ok(Self { airlines })
    }

    /// Name of the airline with the given ICAO designator (case-insensitive), if known.
    /// Used by a multiplayer receiver to turn the portable `airline_icao` from the wire
    /// back into a name it can match against its own installed liveries / titles.
    pub fn name_for_icao(&self, icao: &str) -> Option<&str> {
        if icao.is_empty() {
            return None;
        }
        self.airlines
            .iter()
            .find(|a| a.icao.eq_ignore_ascii_case(icao))
            .map(|a| a.name.as_str())
    }
}

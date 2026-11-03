//! Resolves what a sim aircraft is from the strings the sim gives: its ICAO type designator
//! and its operating airline, from the aircraft title and livery.
//!
//! Sims rarely say this reliably. MSFS `ATC MODEL` and X-Plane `acf_ICAO` are often empty
//! or wrong on add-ons, and neither names an airline. So both are read off the title and
//! livery with word indexes over two reference tables built into this crate.
//!
//! The service resolves every telemetry session with this crate and returns the result when
//! the session opens (docs/sim-native-telemetry.md §6.1). The app forwards that to traffic
//! peers, and resolves locally with the same crate while it has no answer yet.

pub mod aircraft_characteristics;
pub mod airline_index;
pub mod airlines;
pub mod icao_index;

use std::sync::OnceLock;

use aircraft_characteristics::CharacteristicsDatabase;
use airline_index::{AirlineIndex, AirlineMatch};
use airlines::AirlinesDatabase;
use icao_index::{IcaoIndex, IcaoMatch};

/// What an aircraft was resolved to.
#[derive(Debug, Clone, Default)]
pub struct Resolution {
    /// The ICAO type, if anything matched.
    pub icao: Option<IcaoMatch>,
    /// The operating airline, if one was deduced.
    pub airline: Option<AirlineMatch>,
    /// The type is one airlines don't fly (a piston or light turboprop), so no airline was
    /// looked for: a livery word matching an airline there is noise.
    pub airline_skipped: bool,
}

impl Resolution {
    pub fn icao_code(&self) -> &str {
        self.icao.as_ref().map_or("", |m| m.icao.as_str())
    }

    pub fn airline_icao(&self) -> &str {
        self.airline.as_ref().map_or("", |m| m.icao.as_str())
    }

    pub fn airline_name(&self) -> &str {
        self.airline.as_ref().map_or("", |m| m.name.as_str())
    }
}

/// The two indexes, built once over the built-in tables.
pub struct Resolver {
    pub aircraft: &'static CharacteristicsDatabase,
    pub types: IcaoIndex,
    pub airlines: AirlineIndex,
}

impl Resolver {
    /// Built on first use, which takes a moment: call it off any latency-sensitive path.
    pub fn embedded() -> &'static Resolver {
        static RESOLVER: OnceLock<Resolver> = OnceLock::new();
        RESOLVER.get_or_init(|| {
            let aircraft = CharacteristicsDatabase::embedded();
            Resolver {
                aircraft,
                types: IcaoIndex::build(aircraft),
                airlines: AirlineIndex::build(AirlinesDatabase::embedded()),
            }
        })
    }

    /// The title is trusted over the livery for the type, since liveries carry
    /// registrations and studio codes that can spell an unrelated type. The airline is
    /// read from both.
    pub fn resolve(&self, title: &str, livery: &str) -> Resolution {
        let icao = self.types.find_title_livery(title, livery);
        let code = icao.as_ref().map_or("", |m| m.icao.as_str());
        if !self.aircraft.type_may_fly_for_airline(code) {
            return Resolution { icao, airline: None, airline_skipped: true };
        }
        // The livery's own name first: an X-Plane livery is a path through the aircraft's
        // folder, whose name ("DWSim_ATR_72_600_XP12") says nothing about the operator.
        let livery = livery_name(livery);
        let airline = Some(livery)
            .filter(|l| !l.is_empty())
            .and_then(|l| self.airlines.find(l))
            .or_else(|| self.airlines.find(&format!("{title} {livery}")));
        Resolution { icao, airline, airline_skipped: false }
    }
}

/// The last part of a livery path ("Aircraft/.../liveries/Voyager Aviation/"), or the
/// livery as it is (MSFS names it).
fn livery_name(livery: &str) -> &str {
    let livery = livery.trim();
    livery.rsplit(['/', '\\']).find(|s| !s.trim().is_empty()).map_or(livery, str::trim)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_airliner_gets_its_type_and_airline() {
        let r = Resolver::embedded().resolve("Boeing 737-800 Ryanair", "");
        assert_eq!(r.icao_code(), "B738");
        assert_eq!(r.airline_icao(), "RYR");
        assert!(!r.airline_skipped);
    }

    #[test]
    fn the_authors_liveries_are_voyager_aviation() {
        let r = Resolver::embedded();
        let voyager = |title: &str, livery: &str| r.resolve(title, livery).airline_icao().to_string();
        assert_eq!(voyager("ATR 72-600", "Aircraft/DWSim_ATR_72_600_XP12/liveries/Voyager Aviation/"), "VYA");
        assert_eq!(r.resolve("ATR 72-600", "Aircraft/DWSim_ATR_72_600_XP12/liveries/Voyager Aviation/").icao_code(), "AT76");
        assert_eq!(voyager("FSReborn Phenom 300E PRO Oberon Interior", "Voyager Aviation | S1 (2K Dynamic)"), "VYA");
        assert_eq!(voyager("FFX HS125 - 800XP", "Voyager | N850VY"), "VYA");
        assert_eq!(voyager("FFX HS125 - 800XP", "Voyager | Dynamic Registration"), "VYA");
        assert_eq!(voyager("ToLiSs A320 Hi Def", "Aircraft/ToLissA320_V1p2p1/liveries/Voyager A20N N20VY/"), "VYA");
        assert_eq!(voyager("Dassault Falcon 8X by Aerobask", "Aircraft/Falcon 8X/liveries/Voyager/"), "VYA");
    }

    #[test]
    fn a_light_aircraft_gets_no_airline() {
        let r = Resolver::embedded().resolve("Cessna Skyhawk", "United");
        assert_eq!(r.icao_code(), "C172");
        assert!(r.airline.is_none());
        assert!(r.airline_skipped);
    }

    #[test]
    fn nothing_matches_nothing() {
        let r = Resolver::embedded().resolve("", "");
        assert_eq!(r.icao_code(), "");
        assert_eq!(r.airline_icao(), "");
    }
}

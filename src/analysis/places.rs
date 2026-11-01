//! Airports and runways (OurAirports CSVs in `data/`), from the app's `airports.rs` and
//! `runways.rs`: nearest-airport lookup for a flight's endpoints and runways by airport.

use rstar::{PointDistance, RTree, RTreeObject, AABB};
use serde::Deserialize;
use std::collections::HashMap;
use std::error::Error;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct Airport {
    pub ident: String,
    #[serde(rename = "type")]
    pub airport_type: String,
    pub name: String,
    pub latitude_deg: Option<f64>,
    pub longitude_deg: Option<f64>,
    pub elevation_ft: Option<i32>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Runway {
    pub airport_ident: String,
    pub length_ft: Option<i32>,
    pub width_ft: Option<i32>,
    pub le_ident: Option<String>,
    pub le_latitude_deg: Option<f64>,
    pub le_longitude_deg: Option<f64>,
    #[serde(rename = "le_heading_degT")]
    pub le_heading_degt: Option<f64>,
    pub le_displaced_threshold_ft: Option<i32>,
    pub he_ident: Option<String>,
    pub he_latitude_deg: Option<f64>,
    pub he_longitude_deg: Option<f64>,
    #[serde(rename = "he_heading_degT")]
    pub he_heading_degt: Option<f64>,
    pub he_displaced_threshold_ft: Option<i32>,
}

/// An airport's position on the unit sphere, for the R-tree.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Location {
    index: usize,
    coords: [f64; 3],
}

impl RTreeObject for Location {
    type Envelope = AABB<[f64; 3]>;

    fn envelope(&self) -> Self::Envelope {
        AABB::from_point(self.coords)
    }
}

impl PointDistance for Location {
    fn distance_2(&self, point: &[f64; 3]) -> f64 {
        (0..3).map(|i| (self.coords[i] - point[i]).powi(2)).sum()
    }
}

fn cartesian(lat_deg: f64, lon_deg: f64) -> [f64; 3] {
    let (lat, lon) = (lat_deg.to_radians(), lon_deg.to_radians());
    [lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin()]
}

/// When the closest airport is an uncoded placeholder, a coded one within this distance
/// is preferred.
const PREFER_CODED_WITHIN_KM: f64 = 10.0;

/// OurAirports' synthetic `{country}-{n}` idents for fields with no official code
/// ("RS-0011"). Real idents never have that shape.
fn is_placeholder_ident(ident: &str) -> bool {
    let b = ident.as_bytes();
    b.len() >= 4
        && b[0].is_ascii_uppercase()
        && b[1].is_ascii_uppercase()
        && b[2] == b'-'
        && b[3..].iter().all(|c| c.is_ascii_alphanumeric())
}

pub fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const EARTH_RADIUS_KM: f64 = 6371.0;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * a.sqrt().asin()
}

pub struct Places {
    airports: Vec<Airport>,
    by_ident: HashMap<String, usize>,
    tree: RTree<Location>,
    runways: HashMap<String, Vec<Runway>>,
}

impl Places {
    /// `airports.csv` and `runways.csv` in `dir`.
    pub fn load(dir: &Path) -> Result<Self, Box<dyn Error>> {
        let mut airports = Vec::new();
        for airport in csv::Reader::from_path(dir.join("airports.csv"))?.deserialize() {
            airports.push(airport?);
        }
        let mut runways: HashMap<String, Vec<Runway>> = HashMap::new();
        for runway in csv::Reader::from_path(dir.join("runways.csv"))?.deserialize() {
            let runway: Runway = runway?;
            runways.entry(runway.airport_ident.clone()).or_default().push(runway);
        }
        Ok(Self::new(airports, runways))
    }

    pub fn new(airports: Vec<Airport>, runways: HashMap<String, Vec<Runway>>) -> Self {
        let by_ident = airports.iter().enumerate().map(|(i, a)| (a.ident.clone(), i)).collect();
        // Closed fields, heliports and seaplane bases never name a flight's endpoint.
        let locations = airports
            .iter()
            .enumerate()
            .filter(|(_, a)| !matches!(a.airport_type.as_str(), "closed" | "heliport" | "seaplane_base"))
            .filter_map(|(index, a)| {
                Some(Location { index, coords: cartesian(a.latitude_deg?, a.longitude_deg?) })
            })
            .collect();
        Self { airports, by_ident, tree: RTree::bulk_load(locations), runways }
    }

    pub fn airport(&self, ident: &str) -> Option<&Airport> {
        self.by_ident.get(ident).map(|&i| &self.airports[i])
    }

    pub fn runways(&self, ident: &str) -> &[Runway] {
        self.runways.get(ident).map_or(&[], Vec::as_slice)
    }

    fn nearest(&self, lat: f64, lon: f64, count: usize) -> Vec<&Airport> {
        self.tree
            .nearest_neighbor_iter(&cartesian(lat, lon))
            .take(count)
            .map(|loc| &self.airports[loc.index])
            .collect()
    }

    /// The airport at a position, for naming a flight's endpoint: the nearest, unless
    /// that is a placeholder field with a coded airport within
    /// [`PREFER_CODED_WITHIN_KM`].
    pub fn best_airport(&self, lat: f64, lon: f64) -> Option<&Airport> {
        let candidates = self.nearest(lat, lon, 10);
        let closest = *candidates.first()?;
        if !is_placeholder_ident(&closest.ident) {
            return Some(closest);
        }
        let coded = candidates.iter().find(|a| !is_placeholder_ident(&a.ident));
        if let Some(coded) = coded {
            if let (Some(clat), Some(clon)) = (coded.latitude_deg, coded.longitude_deg) {
                if haversine_km(lat, lon, clat, clon) <= PREFER_CODED_WITHIN_KM {
                    return Some(coded);
                }
            }
        }
        Some(closest)
    }
}

#[cfg(test)]
pub(crate) fn mock_airport(ident: &str, lat: f64, lon: f64) -> Airport {
    Airport {
        ident: ident.into(),
        airport_type: "small_airport".into(),
        name: format!("Airport {ident}"),
        latitude_deg: Some(lat),
        longitude_deg: Some(lon),
        elevation_ft: Some(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_a_coded_airport_over_a_colocated_placeholder() {
        // LYBE and the "Surčin Airstrip" placeholder 5.5 km south of it.
        let places = Places::new(
            vec![mock_airport("LYBE", 44.8184, 20.3091), mock_airport("RS-0011", 44.7689, 20.3047)],
            HashMap::new(),
        );
        assert_eq!(places.nearest(44.7689, 20.3047, 1)[0].ident, "RS-0011");
        assert_eq!(places.best_airport(44.7689, 20.3047).unwrap().ident, "LYBE");
    }

    #[test]
    fn keeps_a_placeholder_with_no_coded_airport_near() {
        let places = Places::new(
            vec![mock_airport("RS-0011", 10.0, 10.0), mock_airport("LYBE", 13.0, 13.0)],
            HashMap::new(),
        );
        assert_eq!(places.best_airport(10.0, 10.0).unwrap().ident, "RS-0011");
    }

    #[test]
    fn placeholder_idents() {
        assert!(is_placeholder_ident("RS-0011"));
        assert!(is_placeholder_ident("GB-0AB1"));
        assert!(!is_placeholder_ident("LYBE"));
        assert!(!is_placeholder_ident("X51"));
    }

    #[test]
    fn loads_the_bundled_data() {
        let places = Places::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("data")).unwrap();
        assert_eq!(places.best_airport(46.1580, -123.8787).unwrap().ident, "KAST");
        assert!(places.runways("KAST").len() >= 2);
    }
}

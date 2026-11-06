//! Word → airline index for identifying an operator from a free string (sim aircraft
//! title or livery name, e.g. `"Boeing 737-800 Ryanair"` or a livery path containing
//! `".../United/"`). Same design as [`crate::icao_index`]: an inverted token index built
//! from `airlines.csv`, scored with IDF so distinctive airline words dominate.
//!
//! Keyword sources, in decreasing trust:
//!   1. The airline ICAO designator (e.g. `UAL`) — exact identifier, highest weight.
//!   2. The call sign (e.g. `SPEEDBIRD`).
//!   3. The airline name.
//!
//! Aircraft-manufacturer words and ultra-generic aviation words are dropped as stopwords
//! so a bare model title like `"Airbus A320"` doesn't masquerade as an operator.

use std::collections::{HashMap, HashSet};

use crate::airlines::AirlinesDatabase;
use crate::icao_index::{drop_developer_phrases, ADDON_DEVELOPERS};

const W_ICAO: f32 = 4.0;
const W_CALLSIGN: f32 = 1.5;
const W_NAME: f32 = 1.0;

/// Words removed from both the index and the query. Aircraft manufacturers (so model
/// titles don't match operators) plus generic aviation filler that carries no operator
/// identity on its own.
const STOPWORDS: &[&str] = &[
    // manufacturers
    "boeing", "airbus", "cessna", "embraer", "bombardier", "cirrus", "piper", "diamond",
    "daher", "pilatus", "beechcraft", "beech", "gulfstream", "dassault", "mcdonnell",
    "douglas", "antonov", "tupolev", "sukhoi", "robin", "mooney", "textron", "tecnam",
    "icon", "kodiak", "honda", "hondajet", "asobo", "microsoft",
    // "ATR" is also Atlas Airlines' designator: "ATR 72-600" made every ATR an Atlas one.
    "atr", "saab", "fokker", "lockheed", "havilland", "dhc", "bae", "sikorsky", "eurocopter",
    "mitsubishi", "comac", "ilyushin", "yakovlev", "socata", "grumman", "learjet", "hawker",
    // engine and airframe variant designators — common in livery titles ("A320 CFM WF"),
    // and several collide with real ICAO designators (CFM = ACEF, IAE = AC Insat-Aero).
    "cfm", "cfm56", "iae", "leap", "gtf", "pw", "neo", "ceo",
    // generic aviation filler
    "air", "airline", "airlines", "airway", "airways", "aviation", "aero", "cargo",
    "international", "intl", "express", "transport", "transports", "group", "company",
    "co", "ltd", "inc", "limited", "holdings", "charter", "flight", "flights", "service",
    // English filler, and Antonov's "An-28"
    "services", "and", "the", "of", "an",
    // livery filler: MSFS's "(Dynamic)" and "Dynamic Registration" matched Dynamic Airways
    "dynamic", "registration", "livery", "liveries", "default",
    // model families that are also airline words ("Falcon 50" is not Falcon Air)
    "falcon", "citation", "phenom", "challenger", "caravan", "bluebird",
    // colours: "White A220-300" is a plain livery
    "white", "blue", "red", "green", "black", "grey", "gray", "silver", "gold", "orange",
    // variant notes: "A220-300 - No Cabin", "G3X Avionics", ToLiSs' "A320 Hi Def"
    "no", "cabin", "avionics", "hi", "def",
    // cabin fits: "VIP" is Tag Aviation UK's designator, "EXECUTIVE" Nightexpress' call sign
    "vip", "executive",
];

/// A type designator such as "a320" or "b737": a letter and three digits. It names the
/// aircraft, never its operator, though "SAAD (A320) Limited" carries one.
fn is_type_designator(token: &str) -> bool {
    let mut chars = token.chars();
    token.len() == 4 && chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.all(|c| c.is_ascii_digit())
}

struct Posting {
    airline: u32,
    weight: f32,
}

/// A scored airline candidate for a query.
#[derive(Debug, Clone)]
pub struct AirlineMatch {
    pub icao: String,
    pub name: String,
    pub score: f32,
}

pub struct AirlineIndex {
    icaos: Vec<String>,
    names: Vec<String>,
    /// Each airline's name and call sign as [`phrase`]s, and its name without stopwords
    /// when that leaves two words or more, for [`AirlineIndex::find`]'s preference for an
    /// airline named in full.
    phrases: Vec<[String; 3]>,
    postings: HashMap<String, Vec<Posting>>,
    /// Each airline's ICAO designator, lowercased. Kept apart from the words because a
    /// designator counts only when the query writes it in capitals: "US Airways (New)" is
    /// not Northeastern Aviation (NEW).
    designators: HashMap<String, Vec<u32>>,
    idf: HashMap<String, f32>,
    /// Minimum contribution a *single* token must reach for `find` to accept a match,
    /// scaled to the table size so a match rests on at least one moderately distinctive
    /// word rather than common filler or a pile of weak coincidental words.
    min_score: f32,
}

fn tokenize(s: &str) -> Vec<String> {
    let words = s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .filter(|t| t.len() >= 2 || t.chars().any(|c| c.is_ascii_digit()))
        // A bare number (737, 800, the 28 of "An-28") names no operator either.
        .filter(|t| !STOPWORDS.contains(&t.as_str()) && !is_type_designator(t) && !t.chars().all(|c| c.is_ascii_digit()))
        .collect::<Vec<_>>();
    // Same rule as the type index: a studio name identifies neither the
    // aircraft nor its operator.
    drop_developer_phrases(words)
        .into_iter()
        .filter(|t| !ADDON_DEVELOPERS.contains(&t.as_str()))
        .collect()
}

/// Lowercase words separated by single spaces and padded with one, stopwords kept, so
/// `" voyager aviation "` can be looked for, whole words only, in a query's phrase.
fn phrase(s: &str) -> String {
    let words: Vec<String> = s
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();
    format!(" {} ", words.join(" "))
}

/// `query` without its registrations ("TF-ISN", "EI-DEP", "HB-JCA"): a nationality prefix
/// of one or two letters or digits, a hyphen, and three to five capitals or digits. Their
/// last part spells an airline designator as often as not: Icelandair's TF-ISN is not
/// Interisland Airlines (ISN).
fn without_registrations(query: &str) -> String {
    let is_registration = |word: &str| {
        let word = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '-');
        let caps = |s: &str| s.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
        match word.split_once('-') {
            Some((nation, mark)) => {
                (1..=2).contains(&nation.len())
                    && (3..=5).contains(&mark.len())
                    && caps(nation)
                    && caps(mark)
                    && nation.chars().any(|c| c.is_ascii_uppercase())
                    && mark.chars().any(|c| c.is_ascii_uppercase())
            }
            None => false,
        }
    };
    query
        .split_whitespace()
        .filter(|w| !is_registration(w))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The three-letter words `query` writes in capitals, lowercased: the only ones that can
/// be an airline's designator.
fn capital_designators(query: &str) -> HashSet<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() == 3 && w.chars().all(|c| c.is_ascii_uppercase()))
        .map(str::to_lowercase)
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .collect()
}

fn add_tokens(into: &mut HashMap<String, f32>, text: &str, weight: f32) {
    for token in tokenize(text) {
        into.entry(token)
            .and_modify(|w| *w = w.max(weight))
            .or_insert(weight);
    }
}

impl AirlineIndex {
    pub fn build(db: &AirlinesDatabase) -> Self {
        let mut icaos: Vec<String> = Vec::with_capacity(db.airlines.len());
        let mut names: Vec<String> = Vec::with_capacity(db.airlines.len());
        let mut phrases: Vec<[String; 3]> = Vec::with_capacity(db.airlines.len());
        let mut per: Vec<HashMap<String, f32>> = Vec::with_capacity(db.airlines.len());
        let mut designators: HashMap<String, Vec<u32>> = HashMap::new();

        for (idx, a) in db.airlines.iter().enumerate() {
            let designator = a.icao.trim().to_lowercase();
            if !designator.is_empty() && !STOPWORDS.contains(&designator.as_str()) {
                designators.entry(designator).or_default().push(idx as u32);
            }
            let mut tokens = HashMap::new();
            add_tokens(&mut tokens, &a.callsign, W_CALLSIGN);
            add_tokens(&mut tokens, &a.name, W_NAME);
            icaos.push(a.icao.clone());
            names.push(a.name.clone());
            let words = tokenize(&a.name);
            let stripped = if words.len() >= 2 { format!(" {} ", words.join(" ")) } else { String::new() };
            phrases.push([phrase(&a.name), phrase(&a.callsign), stripped]);
            per.push(tokens);
        }

        let mut postings: HashMap<String, Vec<Posting>> = HashMap::new();
        for (idx, tokens) in per.iter().enumerate() {
            for (token, weight) in tokens {
                postings.entry(token.clone()).or_default().push(Posting {
                    airline: idx as u32,
                    weight: *weight,
                });
            }
        }

        let n = icaos.len().max(1) as f32;
        let idf = postings
            .iter()
            .map(|(token, plist)| (token.clone(), (1.0 + n / plist.len() as f32).ln()))
            .collect();

        // 60% of a unique word's IDF: the single best supporting token must be at least
        // this distinctive. Raised from 40% to cut false operators inferred from words
        // shared by many airlines (e.g. generic "sky"/"wings"-type name fragments).
        let min_score = 0.6 * (1.0 + n).ln();

        Self { icaos, names, phrases, postings, designators, idf, min_score }
    }

    /// Score `query` against every airline, returning `(airline_idx, summed_score,
    /// best_single_token_score)`. The summed score ranks candidates; the best single-token
    /// score is what `find` gates on so a match must rest on one genuinely distinctive word.
    fn score(&self, query: &str) -> Vec<(u32, f32, f32)> {
        let mut sums: HashMap<u32, f32> = HashMap::new();
        let mut bests: HashMap<u32, f32> = HashMap::new();
        let mut seen: HashSet<String> = HashSet::new();
        let query = without_registrations(query);
        let n = self.icaos.len().max(1) as f32;

        for designator in capital_designators(&query) {
            let Some(airlines) = self.designators.get(&designator) else { continue };
            let contrib = (1.0 + n / airlines.len() as f32).ln() * W_ICAO;
            for &airline in airlines {
                *sums.entry(airline).or_insert(0.0) += contrib;
                let best = bests.entry(airline).or_insert(0.0);
                *best = best.max(contrib);
            }
        }

        for token in tokenize(&query) {
            if !seen.insert(token.clone()) {
                continue;
            }
            if let (Some(plist), Some(&idf)) = (self.postings.get(&token), self.idf.get(&token)) {
                for posting in plist {
                    let contrib = idf * posting.weight;
                    *sums.entry(posting.airline).or_insert(0.0) += contrib;
                    let best = bests.entry(posting.airline).or_insert(0.0);
                    if contrib > *best {
                        *best = contrib;
                    }
                }
            }
        }

        sums.into_iter()
            .map(|(idx, sum)| (idx, sum, bests[&idx]))
            .collect()
    }

    pub fn candidates(&self, query: &str, limit: usize) -> Vec<AirlineMatch> {
        let mut matches: Vec<AirlineMatch> = self
            .score(query)
            .into_iter()
            .map(|(idx, score, _best)| AirlineMatch {
                icao: self.icaos[idx as usize].clone(),
                name: self.names[idx as usize].clone(),
                score,
            })
            .collect();
        matches.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.icao.cmp(&b.icao))
        });
        matches.truncate(limit);
        matches
    }

    /// Best airline match for `query`, or `None` if no candidate is supported by a
    /// sufficiently distinctive single word. Among candidates that clear that bar, one
    /// whose whole name is in the query comes first, then one whose call sign or name
    /// without stopwords is, then the rest; within each, the highest summed score wins
    /// (ties broken by ICAO for determinism). Stopwords drop "Aviation" from "Voyager
    /// Aviation", which otherwise ties with Voyager Airlines on "voyager" alone; and
    /// "Atlas Air" is Atlas Air, not Atlas Airlines, whose call sign is "ATLAS-AIR".
    pub fn find(&self, query: &str) -> Option<AirlineMatch> {
        let plain = without_registrations(query);
        let (q, stripped) = (phrase(&plain), format!(" {} ", tokenize(&plain).join(" ")));
        let named = |idx: u32| {
            let [name, callsign, name_words] = &self.phrases[idx as usize];
            let found = |p: &String| p.trim().len() >= 3 && q.contains(p.as_str());
            if found(name) {
                2
            } else if found(callsign) || (!name_words.is_empty() && stripped.contains(name_words.as_str())) {
                1
            } else {
                0
            }
        };
        self.score(query)
            .into_iter()
            .filter(|&(_idx, _sum, best)| best >= self.min_score)
            .map(|(idx, sum, _best)| (idx, sum, named(idx)))
            .max_by(|a, b| {
                a.2.cmp(&b.2)
                    .then_with(|| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                    // Smaller ICAO wins ties: invert so it sorts as the max.
                    .then_with(|| self.icaos[b.0 as usize].cmp(&self.icaos[a.0 as usize]))
            })
            .map(|(idx, score, _named)| AirlineMatch {
                icao: self.icaos[idx as usize].clone(),
                name: self.names[idx as usize].clone(),
                score,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::airlines::Airline;

    fn al(icao: &str, name: &str, callsign: &str) -> Airline {
        Airline { icao: icao.to_string(), name: name.to_string(), callsign: callsign.to_string() }
    }

    fn test_db() -> AirlinesDatabase {
        AirlinesDatabase {
            airlines: vec![
                al("UAL", "United Airlines", "UNITED"),
                al("DLH", "Lufthansa", "LUFTHANSA"),
                al("BAW", "British Airways", "SPEEDBIRD"),
                al("RYR", "Ryanair", "RYANAIR"),
                al("AAL", "American Airlines", "AMERICAN"),
                al("AFR", "Air France", "AIRFRANS"),
                al("VYA", "Voyager Aviation", "VOYAGER"),
                al("EIN", "Aer Lingus", "SHAMROCK"),
                // Contains a word that is also part of an add-on studio name.
                al("SDB", "Sukhoi Design Bureau Company", "SUKHOI"),
                // Real entries whose ICAO designators collide with engine variant
                // designators that show up in livery titles.
                al("CFM", "ACEF", "ACEF"),
                al("IAE", "AC Insat-Aero", ""),
            ],
        }
    }

    fn top(idx: &AirlineIndex, q: &str) -> String {
        idx.find(q).map(|m| m.icao).unwrap_or_default()
    }

    #[test]
    fn matches_airline_name_in_title() {
        let idx = AirlineIndex::build(&test_db());
        assert_eq!(top(&idx, "Boeing 737-800 Ryanair"), "RYR");
        assert_eq!(top(&idx, "Airbus A320 Lufthansa"), "DLH");
        assert_eq!(top(&idx, "Asobo Boeing 747-400 United"), "UAL");
        assert_eq!(top(&idx, "Air France A320"), "AFR");
        assert_eq!(
            top(&idx, "Flying FSReborn Phenom 300E Tristan Interior  Voyager Aviation | S2 (Dynamic) (S108)"),
            "VYA"
        );
        // "CFM" / "IAE" here are engine variants, not the operators whose ICAO
        // designators happen to spell the same thing.
        assert_eq!(
            top(&idx, "Flying FenixA320 CFM WF — Aer Lingus 'Classic' EI-DEP (2023) (A320)"),
            "EIN"
        );
        assert_eq!(top(&idx, "FenixA320 IAE SL — Ryanair"), "RYR");
    }

    /// The livery of the author's ATR, as X-Plane names it.
    #[test]
    fn an_atr_is_not_atlas_airlines() {
        let mut db = test_db();
        db.airlines.push(al("ATR", "Atlas Airlines", "ATLAS-AIR"));
        db.airlines.push(al("VOG", "Voyager Airlines", "VOYAGER AIR"));
        let idx = AirlineIndex::build(&db);
        assert!(idx.find("ATR 72-600").is_none());
        assert_eq!(top(&idx, "ATR 72-600 Aircraft/DWSim_ATR_72_600_XP12/liveries/Voyager Aviation/"), "VYA");
        // The call sign alone, "VOYAGER", is Voyager Aviation's; Voyager Airlines' is
        // "VOYAGER AIR".
        assert_eq!(top(&idx, "Voyager | N850VY"), "VYA");
        assert_eq!(top(&idx, "Voyager Airlines 737"), "VOG");
    }

    /// A registration or a plain word that spells another airline's designator does not
    /// make it the operator, and an airline named in full beats one whose call sign is.
    #[test]
    fn registrations_and_words_are_not_designators() {
        let mut db = test_db();
        db.airlines.extend([
            al("ICE", "Icelandair", "ICEAIR"),
            al("ISN", "Interisland Airlines", "TRI-BIRD"),
            al("AWE", "US Airways", "CACTUS"),
            al("NEW", "Northeastern Aviation", "MEADOW FLIGHT"),
            al("GTI", "Atlas Air", "GIANT"),
            al("ATR", "Atlas Airlines", "ATLAS-AIR"),
        ]);
        let idx = AirlineIndex::build(&db);
        assert_eq!(top(&idx, "[763 GE] Icelandair TF-ISN (Light Blue)"), "ICE");
        assert_eq!(top(&idx, "[752 RR] US Airways (New) - N908AW"), "AWE");
        assert_eq!(top(&idx, "[763 GE] Atlas Air (N641GT)"), "GTI");
        assert!(idx.find("TF-ISN").is_none());
        assert!(idx.find("Boeing 767 New").is_none());
        // A designator in capitals still names its airline.
        assert_eq!(top(&idx, "A320 ISN"), "ISN");
    }

    /// Cabin fits, ToLiSs' "Hi Def" and a type designator name no operator.
    #[test]
    fn cabin_and_model_words_are_not_operators() {
        let mut db = test_db();
        db.airlines.extend([
            al("VIP", "Tag Aviation UK", "SOVEREIGN"),
            al("EXT", "Nightexpress", "EXECUTIVE"),
            al("HFM", "Hi Fly Malta", "MOONRAKER"),
            al("MMS", "SAAD (A320) Limited", "MUSAAD AIR"),
        ]);
        let idx = AirlineIndex::build(&db);
        for title in ["A340-300 VIP EIS2", "Transpacific Executive BBJ N739BB", "ToLiSs A321 Hi Def", "ToLiSs A320 Hi Def"] {
            assert!(idx.find(title).is_none(), "{title}");
        }
        assert_eq!(top(&idx, "Hi Fly Malta A380"), "HFM");
    }

    #[test]
    fn matches_callsign_in_livery() {
        let idx = AirlineIndex::build(&test_db());
        assert_eq!(top(&idx, ".../liveries/SPEEDBIRD retro"), "BAW");
    }

    #[test]
    fn no_airline_in_plain_model_title() {
        let idx = AirlineIndex::build(&test_db());
        // Manufacturer + model only: no operator should be inferred.
        assert!(idx.find("Cessna 172 Skyhawk").is_none());
        assert!(idx.find("Airbus A320").is_none());
    }

    #[test]
    fn weak_shared_words_do_not_sum_into_a_match() {
        // Several airlines share a common, non-distinctive word; on its own it must not
        // infer an operator even though multiple postings accumulate a summed score.
        let db = AirlinesDatabase {
            airlines: vec![
                al("AAA", "Fly Aaa", "ALPHA"),
                al("BBB", "Fly Bbb", "BRAVO"),
                al("CCC", "Fly Ccc", "CHARLIE"),
                al("DDD", "Fly Ddd", "DELTA"),
                al("EEE", "Fly Eee", "ECHO"),
                al("FFF", "Fly Fff", "FOXTROT"),
            ],
        };
        let idx = AirlineIndex::build(&db);
        // "fly" is shared by every airline -> low IDF -> must not clear the bar alone.
        assert!(idx.find("Boeing 737 Fly").is_none());
        // The distinctive name word still resolves its operator.
        assert_eq!(top(&idx, "Boeing 737 Bbb"), "BBB");
    }

    #[test]
    fn ignores_addon_developer_names() {
        let idx = AirlineIndex::build(&test_db());
        // A studio name beside a bare model must not infer an operator.
        assert!(idx.find("Fenix Airbus A320").is_none());
        assert!(idx.find("PMDG Boeing 737-800").is_none());
        // A real operator alongside a studio name still resolves.
        assert_eq!(top(&idx, "PMDG Boeing 737-800 Ryanair"), "RYR");
    }

    #[test]
    fn general_aviation_title_has_no_airline() {
        let idx = AirlineIndex::build(&test_db());
        // A GA aircraft title (developer + model + registration) carries no operator.
        assert!(idx.find("Black Square Baron 58 Professional N515MR").is_none());
        // Model + avionics add-on, still no operator.
        assert!(idx.find("Sting S4 Warmi GTN750").is_none());
        // Ultralight with a parenthesised variant note, still no operator.
        assert!(idx.find("Fox2 MicroFox (912 iS - Low n' Slow)").is_none());
        // Piper M600 should not resolve to an airline
        assert!(idx.find("Piper M600 Firenze Interior").is_none());
    }


    /// An add-on studio in the title is not an operator. "TFDi Design" used to
    /// match Sukhoi Design Bureau on the strength of the word "design" alone,
    /// putting an airline on a flight that had none.
    #[test]
    fn studio_names_do_not_become_operators() {
        let idx = AirlineIndex::build(&test_db());
        assert!(idx.find("TFDi Design MD-11F GE").is_none());
        assert!(idx.find("TFDi Design").is_none());
        // The airline itself must still resolve.
        assert_eq!(idx.find("Sukhoi Design Bureau").map(|m| m.name), Some("Sukhoi Design Bureau Company".to_string()));
    }
}

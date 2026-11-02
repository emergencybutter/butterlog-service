//! Word → ICAO-type index for resolving an ICAO aircraft type designator from a free
//! string (e.g. a sim aircraft title or livery name like `"Asobo Boeing 737-800"`).
//!
//! The index is an inverted map `token -> [(icao, weight)]` built from the aircraft
//! characteristics database, scored with IDF so distinctive tokens (model numbers like
//! `738`) dominate over common ones (manufacturer names like `boeing`).
//!
//! Keyword sources, in decreasing trust:
//!   1. The `ICAO_Code` itself — an exact identifier, so its tokens carry the most weight
//!      (a free string containing `B738` resolves directly).
//!   2. `Model_FAA` and `Model_BADA` — the human model names.
//!   3. `Manufacturer` — brand, the least discriminating on its own.
//!   4. A small curated alias/nickname table (manufacturer spelling variants and common
//!      type nicknames) — the obvious extension point for hand-tuning.

use std::collections::{HashMap, HashSet};

use crate::aircraft_characteristics::CharacteristicsDatabase;

/// Per-field token weights. Identifier-like tokens are far more discriminating than
/// brand/model words, so they are weighted up.
const W_ICAO: f32 = 4.0;
const W_MODEL: f32 = 1.0;
const W_MFG: f32 = 0.6;
const W_ALIAS: f32 = 3.0;

/// Scale applied to livery tokens relative to title tokens; see
/// [`IcaoIndex::find_title_livery`].
const LIVERY_FACTOR: f32 = 0.25;

/// Token-level synonyms applied to both the index and the query, so different spellings
/// of the same brand collapse together. Keep entries that are unambiguous.
const ALIASES: &[(&str, &str)] = &[
    ("beechcraft", "beech"),
    ("mcdonnelldouglas", "mcdonnell"),
    ("aerospatiale", "airbus"),
    ("eurocopter", "airbus"),
];

/// Curated marketing nicknames that do not appear in the characteristics columns,
/// mapped to an ICAO code. Only applied when the code exists in the loaded database.
const NICKNAMES: &[(&str, &str)] = &[
    ("jumbo jet", "B744"),
    ("dreamliner", "B788"),
    // Both Super Hornet spellings map to F18S; F18H is the plain Hornet. The "FA18E" form
    // needs a nickname since the old code-prefix match (fa18e -> fa18) no longer applies now
    // that the code is F18S.
    ("super hornet", "F18S"),
    ("FA18E", "F18S"),
    ("warthog", "A10"),
    ("Long-EZ", "LGEZ"),
    ("Fox2", "FOX"),
    ("UH-1H", "UH1"),
    ("M500", "P46T"),
    ("Sting", "TL20"),
    ("Vision", "SF50"),
    // Joined 737 MAX variant shorthands (e.g. a title "737-MAX8"): the model columns store
    // "MAX" and "8" as separate tokens, so the glued form would otherwise match nothing
    // distinctive and fall back to an arbitrary 737.
    ("MAX7", "B37M"),
    ("MAX8", "B38M"),
    ("MAX9", "B39M"),
    // Add-on "737-800X" titles: the glued "800x" matches no model token, leaving only
    // "737", which ties across the whole family and falls to an arbitrary variant.
    ("800X", "B738"),
    // Other glued model/variant shorthands the model columns store split or under a
    // different code, so the joined query token would otherwise match nothing: the ATR
    // family ("ATR72" vs code AT72), the neo suffix (the code is A20N/A21N, not A320/A321),
    // and the FlyByWire-style "A380X" marketing name.
    ("ATR72", "AT72"),
    // The DB models the ATR 42 only as specific variants (AT43-AT46); a bare "ATR42" with no
    // variant resolves to the current-production -600 (AT46).
    ("ATR42", "AT46"),
    ("A320neo", "A20N"),
    ("A321neo", "A21N"),
    ("A380X", "A388"),
    // Glued/hyphenated forms whose distinctive token the model columns store differently:
    // the C-17's "C-17A" splits to a bare "17a", the Cessna 401 shares the 402's code, and
    // CubCrafters' nosewheel XCub ("NXCub") is one token the model name ("XCub") doesn't cover.
    ("C-17A", "C17"),
    ("C401", "C402"),
    ("NXCub", "CC19"),
    // King Air C90GTx variant: the model columns store the type as "King Air 90"/"E90", so
    // the distinctive "C90GTX" marketing token wouldn't otherwise resolve to the BE9L code.
    ("C90GTX", "BE9L"),
    // Sim titles that drop a model-suffix letter so the bare number matches no column token:
    // Tecnam's "P2006" (type P2006T -> P06T) and Saab's "S2000" (the 2000 -> SB20).
    ("P2006", "P06T"),
    ("S2000", "SB20"),
    // Variant/marketing/add-on shorthands whose distinctive token the model columns don't
    // carry: livery designations (S340B), add-on model names (FSR500 = M500, HA420 = HondaJet,
    // CRJ550 = CRJ-700, B73X = FSLTL's generic 737, FreedomFox = Kitfox), developer shorthands
    // (FWB = FlyByWire A320neo), generic glider models (DG1001E, LS8), and a powered parachute.
    ("S340B", "SF34"),
    ("FSR500", "P46T"),
    ("HA420", "HDJT"),
    ("CRJ550", "CRJ7"),
    ("B73X", "B737"),
    ("FreedomFox", "FOX"),
    ("FWB", "A20N"),
    ("DG1001E", "GLID"),
    ("LS8", "GLID"),
    ("Powrachute", "PARA"),
    // "Vertigo" is an add-on livery/edition name for the Lancair Legacy (LEG2); the model
    // columns carry "Lancair Legacy", which the title doesn't mention.
    ("Vertigo", "LEG2"),
    // The ERJ-140 shares the ERJ-135's type code (the DB lists E135 as "ERJ 135/140"); the
    // glued "E140" title token matches neither the code nor the split "140" model token.
    ("E140", "E135"),
    // The MD-10 is a glass-cockpit DC-10 conversion and shares the DC10 type code.
    ("MD10", "DC10"),
    // Air Tractor AT-802 (existing AT8T) and the Skyship 600 airship: glued title tokens the
    // model columns store split ("AT-802" -> at,802; "Skyship 600" -> skyship,600).
    ("AT802", "AT8T"),
    ("Skyship600", "SKS6"),
    // E175 -> the short-wing variant (E75S, existing); CAP 10 -> CP10; the Maule MT-7 shares
    // the M-7 code; and "S12-G" is a generic glider whose name the model columns don't carry.
    ("E175", "E75S"),
    ("CAP10", "CP10"),
    ("MT7", "M7"),
    ("S12-G", "GLID"),
    // Phenom 300E: the glued "300E" matches no model token, leaving "Phenom" tied between
    // the 100 (E50P) and the 300 (E55P).
    ("300E", "E55P"),
];

/// Weaker nicknames for words the model columns already share across several types, used
/// to break the tie toward the type a bare word most likely means in a sim title. Weighted
/// just above a model word, so a title that also carries a distinguishing model token (e.g.
/// "Embraer Legacy 600") still resolves to that type.
const W_WEAK_ALIAS: f32 = 1.5;
const WEAK_NICKNAMES: &[(&str, &str)] = &[
    // Sim titles like "LEGACY G3X GTN750 TURBO" are the Lancair Legacy; the Embraer
    // bizjets also listing "Legacy" (E135, E35L, E545, E550) carry a model number or brand.
    ("Legacy", "LEG2"),
];

/// Third-party add-on developer / studio names (MSFS & X-Plane) that frequently appear in
/// aircraft titles and livery paths but identify neither the aircraft type nor the operating
/// airline. Dropped during tokenization for both the ICAO-type and airline indexes.
pub const ADDON_DEVELOPERS: &[&str] = &[
    "ifly", "blacksquare", "fsreborn", "pmdg", "fenix", "flightsimware",
    "asobo", "fsltl", "passiveaircraft", "laminar", "tfdi",
];

/// Studio names made of words that are meaningful on their own, so they can
/// only be dropped as a phrase.
///
/// "TFDi Design" is the case that forced this: dropping the bare word `design`
/// would also blind the index to Flight Design, a real manufacturer, and to
/// Sukhoi Design Bureau on the airline side. Left in, it out-scores the actual
/// model - "TFDi Design MD-11F GE" resolved to a Flight Design CT and an
/// airline nobody flew.
pub const ADDON_DEVELOPER_PHRASES: &[&[&str]] = &[&["tfdi", "design"]];

/// Remove any run of tokens matching a studio phrase, left to right.
pub fn drop_developer_phrases(tokens: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let hit = ADDON_DEVELOPER_PHRASES.iter().find(|phrase| {
            tokens[i..].len() >= phrase.len()
                && phrase.iter().enumerate().all(|(k, w)| tokens[i + k] == *w)
        });
        match hit {
            Some(phrase) => i += phrase.len(),
            None => {
                out.push(tokens[i].clone());
                i += 1;
            }
        }
    }
    out
}

/// Shortest ICAO code length considered when matching a code as the prefix of a word.
const MIN_CODE_PREFIX_LEN: usize = 3;

struct Posting {
    icao: u32,
    weight: f32,
}

/// A scored ICAO-type candidate for a query.
#[derive(Debug, Clone)]
pub struct IcaoMatch {
    pub icao: String,
    pub score: f32,
}

pub struct IcaoIndex {
    icaos: Vec<String>,
    postings: HashMap<String, Vec<Posting>>,
    idf: HashMap<String, f32>,
    /// Lowercased set of all ICAO codes, for prefix matching against query words.
    code_set: HashSet<String>,
}

/// Split a free string into normalized tokens: lowercased, broken on any non-alphanumeric
/// character, applying alias canonicalization. Pure single-letter tokens are dropped as
/// noise, but short tokens containing a digit (e.g. `8`, `a320`) are kept.
fn tokenize(s: &str) -> Vec<String> {
    // Phrases are dropped before the single-word stoplist, so a phrase whose
    // first word is also a lone studio name still matches as a phrase.
    let words: Vec<String> = s
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(strip_addon_prefix)
        .map(canonicalize)
        .filter(|t| t.len() >= 2 || t.chars().any(|c| c.is_ascii_digit()))
        .collect();
    drop_developer_phrases(words)
        .into_iter()
        .filter(|t| !ADDON_DEVELOPERS.contains(&t.as_str()))
        .collect()
}

/// Strip a leading add-on developer/studio name glued to a model token (e.g. a title
/// "FenixA321" tokenizes to "fenixa321" -> "a321"), so the embedded type still resolves.
/// Returns the token unchanged when it doesn't start with a known developer name, or is
/// exactly one (those are dropped by the stopword filter instead).
fn strip_addon_prefix(token: &str) -> &str {
    for dev in ADDON_DEVELOPERS {
        if token.len() > dev.len() && token.starts_with(dev) {
            return &token[dev.len()..];
        }
    }
    token
}

fn canonicalize(token: impl Into<String>) -> String {
    let token = token.into();
    ALIASES
        .iter()
        .find(|(from, _)| *from == token)
        .map(|(_, to)| to.to_string())
        .unwrap_or(token)
}

/// Add every token of `text` to `into`, keeping the strongest weight when a token shows
/// up via several fields.
fn add_tokens(into: &mut HashMap<String, f32>, text: &str, weight: f32) {
    for token in tokenize(text) {
        into.entry(token)
            .and_modify(|w| *w = w.max(weight))
            .or_insert(weight);
    }
}

impl IcaoIndex {
    /// Build the index from an already-loaded characteristics database.
    pub fn build(db: &CharacteristicsDatabase) -> Self {
        let mut icaos: Vec<String> = Vec::with_capacity(db.characteristics.len());
        let mut per_icao: Vec<HashMap<String, f32>> = Vec::with_capacity(db.characteristics.len());
        let mut by_code: HashMap<String, usize> = HashMap::new();

        for c in db.characteristics.values() {
            if c.icao_code.is_empty() {
                continue;
            }
            let idx = icaos.len();
            by_code.insert(c.icao_code.to_lowercase(), idx);
            icaos.push(c.icao_code.clone());

            let mut tokens = HashMap::new();
            add_tokens(&mut tokens, &c.icao_code, W_ICAO);
            add_tokens(&mut tokens, &c.manufacturer, W_MFG);
            add_tokens(&mut tokens, &c.model_faa, W_MODEL);
            add_tokens(&mut tokens, &c.model_bada, W_MODEL);
            per_icao.push(tokens);
        }

        // Other source: curated nicknames, applied only when the target code exists.
        for (nick, code) in NICKNAMES {
            if let Some(&idx) = by_code.get(&code.to_lowercase()) {
                add_tokens(&mut per_icao[idx], nick, W_ALIAS);
            }
        }
        for (nick, code) in WEAK_NICKNAMES {
            if let Some(&idx) = by_code.get(&code.to_lowercase()) {
                add_tokens(&mut per_icao[idx], nick, W_WEAK_ALIAS);
            }
        }

        // Invert into postings.
        let mut postings: HashMap<String, Vec<Posting>> = HashMap::new();
        for (idx, tokens) in per_icao.iter().enumerate() {
            for (token, weight) in tokens {
                postings.entry(token.clone()).or_default().push(Posting {
                    icao: idx as u32,
                    weight: *weight,
                });
            }
        }

        // IDF: tokens shared by few types are the most informative.
        let n = icaos.len().max(1) as f32;
        let idf = postings
            .iter()
            .map(|(token, plist)| (token.clone(), (1.0 + n / plist.len() as f32).ln()))
            .collect();

        let code_set: HashSet<String> = by_code.keys().cloned().collect();

        Self { icaos, postings, idf, code_set }
    }

    /// Accumulate IDF-weighted postings for one token, counting each token at most once.
    fn accumulate(&self, token: &str, scores: &mut HashMap<u32, f32>, seen: &mut HashSet<String>) {
        if !seen.insert(token.to_string()) {
            return;
        }
        if let (Some(plist), Some(&idf)) = (self.postings.get(token), self.idf.get(token)) {
            for posting in plist {
                *scores.entry(posting.icao).or_insert(0.0) += idf * posting.weight;
            }
        }
    }

    /// If an ICAO code is an exact, *proper* prefix of `token` and the trailing suffix
    /// does not contain a `neo`/`max` variant marker, return that code (the longest such).
    /// e.g. `b738w` -> `b738`, `dr400` -> `dr40`; but `a320neo` / `b737max` -> `None`.
    fn code_prefix_of(&self, token: &str) -> Option<String> {
        if !token.is_ascii() {
            return None;
        }
        let n = token.len();
        // Code length `i` is a proper prefix (suffix non-empty), longest first.
        for i in (MIN_CODE_PREFIX_LEN..n).rev() {
            let prefix = &token[..i];
            if self.code_set.contains(prefix) {
                let suffix = &token[i..];
                if suffix.contains("neo") || suffix.contains("max") {
                    return None;
                }
                return Some(prefix.to_string());
            }
        }
        None
    }

    /// Return up to `limit` ICAO candidates for `query`, best score first.
    pub fn candidates(&self, query: &str, limit: usize) -> Vec<IcaoMatch> {
        self.ranked(&[(query, 1.0)], limit)
    }

    /// Score each `(text, factor)` part, scaling its tokens by `factor`. A token counts
    /// once, at the factor of the first part it appears in.
    fn ranked(&self, parts: &[(&str, f32)], limit: usize) -> Vec<IcaoMatch> {
        let mut scores: HashMap<u32, f32> = HashMap::new();
        let mut seen: HashSet<String> = HashSet::new();

        for (text, factor) in parts {
            let mut part_scores: HashMap<u32, f32> = HashMap::new();
            for token in tokenize(text) {
                self.accumulate(&token, &mut part_scores, &mut seen);
                // A type code that is an exact prefix of the word (with a non-neo/max suffix)
                // implies that exact type — credit it as if the code appeared verbatim.
                if let Some(code) = self.code_prefix_of(&token) {
                    self.accumulate(&code, &mut part_scores, &mut seen);
                }
            }
            for (idx, score) in part_scores {
                *scores.entry(idx).or_insert(0.0) += score * factor;
            }
        }

        let mut matches: Vec<IcaoMatch> = scores
            .into_iter()
            .map(|(idx, score)| IcaoMatch {
                icao: self.icaos[idx as usize].clone(),
                score,
            })
            .collect();
        matches.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                // Stable tiebreak so equal scores are deterministic.
                .then_with(|| a.icao.cmp(&b.icao))
        });
        matches.truncate(limit);
        matches
    }

    /// Best single ICAO match for `query`, or `None` if nothing matched.
    pub fn find(&self, query: &str) -> Option<IcaoMatch> {
        self.candidates(query, 1).into_iter().next()
    }

    /// Best ICAO match for a sim aircraft, trusting the title over the livery. Livery
    /// names carry registrations and studio livery codes that can spell an unrelated type
    /// (FSReborn's "... (S108)" is a Stinson 108's code), so their tokens count for a
    /// fraction; they still decide when the title has no type signal of its own.
    pub fn find_title_livery(&self, title: &str, livery: &str) -> Option<IcaoMatch> {
        self.ranked(&[(title, 1.0), (livery, LIVERY_FACTOR)], 1).into_iter().next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aircraft_characteristics::AircraftCharacteristic;

    fn ac(icao: &str, mfg: &str, faa: &str, bada: &str) -> AircraftCharacteristic {
        AircraftCharacteristic {
            icao_code: icao.to_string(),
            manufacturer: mfg.to_string(),
            model_faa: faa.to_string(),
            model_bada: bada.to_string(),
            engine_type: String::new(),
            num_engines: 0,
            wtc: String::new(),
            class: String::new(),
            wingspan: 0.0,
        }
    }

    fn test_db() -> CharacteristicsDatabase {
        let rows = [
            ac("A320", "AIRBUS", "Airbus A320", "Airbus A320-231"),
            ac("A321", "AIRBUS", "Airbus A321", "Airbus A321-231"),
            ac("A20N", "AIRBUS", "Airbus A320 Neo", "Airbus A320-271N"),
            ac("A21N", "AIRBUS", "Airbus A321 Neo", "Airbus A321-251N"),
            ac("A388", "AIRBUS", "Airbus A380-800", "Airbus A380-841"),
            ac("AT72", "ATR", "ATR-72-201/202", "ATR 72-200"),
            ac("B738", "BOEING", "Boeing 737-800", "Boeing B737-800"),
            ac("B737", "BOEING", "Boeing 737-700", "Boeing B737-700"),
            ac("B733", "BOEING", "Boeing 737-300", "Boeing B737-300"),
            ac("B38M", "BOEING", "Boeing 737 MAX 8", "Boeing B737-8 Max"),
            ac("B744", "BOEING", "Boeing 747-400", "Boeing B747-400"),
            ac("C172", "CESSNA", "Cessna Skyhawk 172/Cutlass", "Cessna 172S Skyhawk SP"),
            ac("C414", "CESSNA", "Cessna 414", "Cessna 414 Chancellor"),
            ac("PC12", "PILATUS", "Pilatus PC-12", "Pilatus PC-12/45"),
            ac("BE10", "BEECH", "Beech King Air 100", "Beech 100 King Air"),
            ac("DR40", "ROBIN", "Robin Regent", "Robin Regent"),
            ac("FOX", "DENNEY", "Kitfox", "Kitfox"),
            ac("CC19", "CUBCRAFTERS", "CubCrafters XCub", "CubCrafters CC19-180 XCub"),
            ac("SREY", "PROGRESSIVE AERODYNE", "SeaRey", "Progressive Aerodyne SeaRey"),
            ac("C408", "CESSNA", "Cessna 408 SkyCourier", "Cessna 408 SkyCourier"),
            ac("M600", "PIPER", "Piper M600", "Piper PA-46-600TP M600"),
            ac("TL20", "TL ULTRALIGHT", "TL Ultralight Sting S4", "TL-2000 Sting"),
            ac("C17", "BOEING", "Boeing Globemaster III", "Boeing C-17 Globemaster III"),
            ac("DC10", "BOEING-MCDONNELL DOUGLAS", "Boeing (Douglas) DC 10-10/30/40", "McDonnell Douglas DC10-30"),
            ac("MD11", "BOEING-MCDONNELL DOUGLAS", "Boeing (Douglas) MD-11", "McDonnell Douglas MD-11F"),
            ac("FDCT", "FLIGHT DESIGN", "Flight Design CT", "Flight Design CT"),
            ac("PC6", "PILATUS", "Pilatus PC-6 Porter", "Pilatus PC-6/B2-H4"),
            ac("DA62", "DIAMOND", "Diamond DA62", "Diamond DA-62"),
            ac("DJET", "DIAMOND", "Diamond D-Jet", "Diamond D-JET"),
            ac("BE17", "BEECH", "Beech 17 Staggerwing", "Beechcraft D17 Staggerwing"),
            ac("C402", "CESSNA", "Cessna 401/402", "Cessna 402B"),
            ac("BE9L", "BEECH", "Beech King Air 90", "Beechcraft King Air E90"),
            ac("F18H", "BOEING", "F/A-18 Hornet", "Boeing F/A-18C/D Hornet"),
            ac("LGEZ", "RUTAN", "Rutan Long-EZ", "Rutan Model 61 Long-EZ"),
            ac("UH1", "BELL", "Bell UH-1 Iroquois", "Bell UH-1H Huey"),
            ac("AT46", "ATR", "ATR 42-600", "ATR 42-600"),
            ac("P06T", "TECNAM", "Tecnam P2006T", "Tecnam P2006T"),
            ac("GLID", "GENERIC", "Generic Glider", "Generic Glider"),
            ac("MXS", "MX AIRCRAFT", "MXS", "MX Aircraft MXS"),
            ac("SAVG", "ZLIN AVIATION", "Savage Cub", "Zlin Savage"),
            ac("P208", "TECNAM", "Tecnam P2008", "Tecnam P2008JC"),
            ac("ECHO", "TECNAM", "Tecnam P92 Echo", "Tecnam P92"),
            ac("SB20", "SAAB", "Saab 2000", "Saab 2000"),
            ac("SF34", "SAAB", "Saab SF 340", "Saab SF 340B"),
            ac("P46T", "PIPER", "Piper Malibu Meridian", "Piper PA-46-500TP"),
            ac("HDJT", "HONDA", "HONDA HA-420 HondaJet", "Honda HA-420 HondaJet"),
            ac("CRJ7", "CANADAIR", "canadair CRJ-700", "Bombardier CRJ-700"),
            ac("PIVI", "PIPISTREL", "Pipistrel Virus SW", "Pipistrel Virus SW 121"),
            ac("PITA", "PIPISTREL", "Pipistrel Taurus", "Pipistrel Taurus M"),
            ac("F18S", "BOEING", "FA-18E/F Super Hornet", "Boeing F/A-18E/F Super Hornet"),
            ac("F7", "FOKKER", "Fokker F.VIIa/3m", "Fokker F7"),
            ac("OPCA", "EDGLEY", "Edgley Optica", "Edgley EA-7 Optica"),
            ac("EDGE", "ZIVKO", "Zivko Edge 540", "Zivko Edge 540"),
            ac("VL3", "JMB", "JMB VL-3", "JMB Aircraft VL-3"),
            ac("CD2", "DORNIER", "Dornier Seastar", "Dornier Seastar CD2"),
            ac("E135", "EMBRAER", "Embraer ERJ 135/140/Legacy", "Embraer EMB-135LR"),
            ac("E35L", "EMBRAER", "Embraer 135BJ Legacy 600", "Embraer EMB-135BJ Legacy 600"),
            ac("E545", "EMBRAER", "Embraer EMB-545 Legacy 450", "Embraer EMB-545 Legacy 450"),
            ac("E550", "EMBRAER", "Legacy 500/Praetor 600", "Embraer EMB-550 Legacy 500"),
            ac("TNDR", "GOT FRIENDS", "Got Friends Tundra", "Tundra"),
            ac("LEG2", "LANCAIR", "Lancair Legacy", "Lancair Legacy 2000"),
            ac("P212", "TECNAM", "Tecnam P2012 Traveller", "Tecnam P2012"),
            ac("C336", "CESSNA", "Cessna 336 Skymaster", "Cessna 336"),
            ac("M7", "MAULE", "Maule M-7", "Maule M-7-235"),
            ac("M9", "MAULE", "Maule M-9", "Maule M-9-235"),
            ac("PARA", "GENERIC", "Powered Parachute", "Powered Parachute"),
            ac("L39", "AERO VODOCHODY", "Aero L-39 Albatros", "Aero L-39 Albatros"),
            ac("DA20", "DIAMOND", "Diamond DA20 Katana", "Diamond DA20-C1 Eclipse"),
            ac("C82S", "CESSNA", "Cessna T182 Skylane", "Cessna T182T Turbo Skylane"),
            ac("SIRA", "TECNAM", "Tecnam P2002 Sierra", "Tecnam P2002-JF Sierra"),
            ac("SKS6", "AIRSHIP INDUSTRIES", "Skyship 600", "Skyship 600"),
            ac("BRAV", "TECNAM", "Tecnam P2004 Bravo", "Tecnam P2004 Bravo"),
            ac("C337", "CESSNA", "Cessna 337 Skymaster", "Cessna 337 Super Skymaster"),
            ac("T50", "CESSNA", "Cessna T-50 Bobcat", "Cessna AT-17 Bobcat"),
            ac("AT8T", "AIR TRACTOR", "Air Tractor AT-802", "ATR AT-802"),
            ac("MM24", "MAGNI GYRO", "Magni M24 Orion", "Magni M24"),
            ac("C411", "CESSNA", "Cessna 411", "Cessna 411A"),
            ac("C82T", "CESSNA", "Cessna TR182 Turbo Skylane RG", "Cessna TR182 Skylane RG"),
            ac("TWEN", "TECNAM", "Tecnam P2010 Twenty", "Tecnam P2010"),
            ac("C205", "CESSNA", "Cessna 205", "Cessna 205A"),
            ac("CP10", "CAP AVIATION", "Mudry CAP 10", "CAP Aviation CAP-10B"),
            ac("U16", "GRUMMAN", "Grumman HU-16 Albatross", "Grumman HU-16 Albatross"),
            ac("JAS4", "JOBY AVIATION", "Joby Aviation S4", "Joby S4"),
            ac("E75S", "EMBRAER", "Embraer 175 short wing", "Embraer ERJ 170-200 IGW"),
            ac("E50P", "EMBRAER", "Embraer 500 Phenom 100", "Embraer EMB-500 Phenom 100"),
            ac("E55P", "EMBRAER", "Embraer Phenom 300", "Embraer EMB-505 Phenom 300"),
            ac("S108", "STINSON", "Stinson 108 Voyager", "Stinson 108 Voyager"),
        ];
        let mut characteristics = HashMap::new();
        for r in rows {
            characteristics.insert(r.icao_code.clone(), r);
        }
        CharacteristicsDatabase { characteristics }
    }

    fn top(idx: &IcaoIndex, q: &str) -> String {
        idx.find(q).map(|m| m.icao).unwrap_or_default()
    }

    #[test]
    fn resolves_from_model_names() {
        let idx = IcaoIndex::build(&test_db());
        assert_eq!(top(&idx, "Boeing 737-800"), "B738");
        assert_eq!(top(&idx, "Boeing 737-700"), "B737");
        assert_eq!(top(&idx, "Airbus A320"), "A320");
        assert_eq!(top(&idx, "Cessna 172"), "C172");
        assert_eq!(top(&idx, "Pilatus PC-12"), "PC12");
        assert_eq!(top(&idx, "Beech King Air 100"), "BE10");
        assert_eq!(top(&idx, "Piper M600 Firenze Interior"), "M600");
    }

    #[test]
    fn resolves_from_embedded_icao_code() {
        let idx = IcaoIndex::build(&test_db());
        // The ICAO code appearing as a standalone token resolves directly.
        assert_eq!(top(&idx, "FSLabs A320 Lufthansa"), "A320");
        assert_eq!(top(&idx, "PMDG B738 American"), "B738");
    }

    #[test]
    fn handles_noisy_titles() {
        let idx = IcaoIndex::build(&test_db());
        assert_eq!(top(&idx, "Asobo Boeing 747-400 Atlas Air"), "B744");
    }

    #[test]
    fn nickname_source() {
        let idx = IcaoIndex::build(&test_db());
        assert_eq!(top(&idx, "the queen, a jumbo jet"), "B744");
    }

    #[test]
    fn code_as_prefix_of_word_implies_type() {
        let idx = IcaoIndex::build(&test_db());
        assert_eq!(top(&idx, "B738W"), "B738");      // winglets suffix
        assert_eq!(top(&idx, "DR400"), "DR40");      // code DR40 is a prefix of dr400
        assert_eq!(top(&idx, "C414AW"), "C414");     // suffix "aw"
        assert_eq!(top(&idx, "PMDG B738BCF American"), "B738");
    }

    #[test]
    fn resolves_joined_max_variant() {
        let idx = IcaoIndex::build(&test_db());
        // A glued "MAX8" suffix should pin the specific MAX variant, not an arbitrary 737.
        assert_eq!(top(&idx, "iFly 737-MAX8 Alaska Airlines N801AK (178Seat)"), "B38M");
        // Likewise a glued "800X" suffix pins the -800.
        assert_eq!(top(&idx, "Boeing 737-800X"), "B738");
    }

    #[test]
    fn resolves_nickname_among_noise() {
        let idx = IcaoIndex::build(&test_db());
        // "Fox2" is a curated nickname for the Kitfox (FOX); the surrounding model/variant
        // noise carries no competing type signal.
        assert_eq!(top(&idx, "Fox2 KY56 (915 iS)"), "FOX");
    }

    #[test]
    fn strips_developer_name_glued_to_model() {
        let idx = IcaoIndex::build(&test_db());
        // Add-on developer name fused to the model (no separator) still resolves the type.
        assert_eq!(top(&idx, "FenixA320 IAE SL"), "A320");
        assert_eq!(top(&idx, "FenixA321 IAE WF TC"), "A321");
        assert_eq!(top(&idx, "FenixA321 IAE SL TC"), "A321");
    }

    #[test]
    fn resolves_glued_variant_shorthands() {
        let idx = IcaoIndex::build(&test_db());
        // Joined model/variant forms whose tokens the data stores split or under another code.
        assert_eq!(top(&idx, "Asobo PassiveAircraft ATR72-200F"), "AT72");
        assert_eq!(top(&idx, "Pride A380X"), "A388");
        assert_eq!(top(&idx, "A320neo V2"), "A20N");
        // A code that is a proper prefix of a token (with a benign suffix) still resolves.
        assert_eq!(top(&idx, "FSLTL_FAIB_B733F_Clever_Cargo"), "B733");
    }

    #[test]
    fn resolves_light_ga_types() {
        let idx = IcaoIndex::build(&test_db());
        // Light GA / experimental types resolve from a distinctive model word or the code.
        assert_eq!(top(&idx, "XCub Passengers Skis"), "CC19");
        assert_eq!(top(&idx, "SeaRey Elite Green (Factory Build)"), "SREY");
        assert_eq!(top(&idx, "SeaRey Elite White (Factory Build)"), "SREY");
        assert_eq!(top(&idx, "Microsoft PassiveAircraft C408 Passenger"), "C408");
    }

    #[test]
    fn neo_and_max_variants_resolve_to_their_codes() {
        let idx = IcaoIndex::build(&test_db());
        // The neo variant has its own code; the glued form resolves to it (not base A320).
        assert_eq!(top(&idx, "A320neo"), "A20N");
        // ...but a bare "MAX" with no variant number stays unresolved (ambiguous between
        // MAX 7/8/9), rather than being assumed to be the base 737.
        assert!(idx.find("B737MAX").is_none());
    }

    #[test]
    fn no_match_returns_none() {
        let idx = IcaoIndex::build(&test_db());
        assert!(idx.find("spaceship zzzqqq").is_none());
    }



    #[test]
    fn resolves_real_world_sim_titles() {
        let idx = IcaoIndex::build(&test_db());

        // Kitfox add-on ("Fox2 ...") — the curated nickname carries the type; the trailing
        // engine/variant noise (915 iS, KR699, STOL, livery names) has no competing signal.
        assert_eq!(top(&idx, "Fox2 Mountain Fox (915 iS - Analog)"), "FOX");
        assert_eq!(top(&idx, "Fox2 STOL Club"), "FOX");
        assert_eq!(top(&idx, "Fox2 ElectricProwl (912 iS - Low n' Slow Analog)"), "FOX");
        assert_eq!(top(&idx, "Fox2 KR699 (912 iS - Low n' Slow)"), "FOX");
        assert_eq!(top(&idx, "Fox2 Hwite Fox (912 iS - Low n' Slow)"), "FOX");
        assert_eq!(top(&idx, "Fox2 Wildflowers (912 iS - Low n' Slow)"), "FOX");

        // TL Ultralight Sting — curated nickname.
        assert_eq!(top(&idx, "Sting S4 Orange Black GTN750"), "TL20");
        assert_eq!(top(&idx, "Sting S4 Bronze Burgundy GTN750"), "TL20");

        // Hyphenated C-17A: tokenizes to a bare "17a", pinned via nickname.
        assert_eq!(top(&idx, "C-17A Military Airlift"), "C17");

        // Cessna 401 shares the 402's ICAO type; glued "C401" pinned via nickname.
        assert_eq!(top(&idx, "Asobo PassiveAircraft C401"), "C402");

        // CubCrafters nosewheel XCub: one token the model name ("XCub") doesn't cover.
        assert_eq!(top(&idx, "NXCub"), "CC19");

        // Codes appearing verbatim as a title token resolve directly off the ICAO column.
        assert_eq!(top(&idx, "Asobo PassiveAircraft MD10-30F"), "DC10"); // MD-10 folds into DC10
        assert_eq!(top(&idx, "Asobo PassiveAircraft PC6"), "PC6");
        assert_eq!(top(&idx, "DA62 Passengers"), "DA62");
        assert_eq!(top(&idx, "Asobo PassiveAircraft DJET"), "DJET");

        // Distinctive model tokens carry the type when the code isn't spelled out: the
        // Staggerwing's "D17" and the King Air's "C90GTx" variant designation.
        assert_eq!(top(&idx, "Microsoft PassiveAircraft D17"), "BE17");
        assert_eq!(top(&idx, "Microsoft PassiveAircraft C90GTX Medic"), "BE9L");

        // Curated nicknames for marketing/variant names the model columns don't carry.
        assert_eq!(top(&idx, "F/A-18E Super Hornet VFA-103"), "F18S");
        assert_eq!(top(&idx, "Long-EZ Experimental"), "LGEZ");
        assert_eq!(top(&idx, "Bell UH-1H Iroquois"), "UH1");
        // ATR 42-600 resolves to its own variant code (AT46), not the ATR 72 (shared "atr"
        // token) nor a generic AT42 (which isn't a real designator — the DB uses AT43-AT46).
        assert_eq!(top(&idx, "ATR 42-600 Passengers"), "AT46");

        // Codes verbatim, code-as-prefix, model words, and suffix-dropping nicknames.
        assert_eq!(top(&idx, "Asobo PassiveAircraft P2006"), "P06T"); // nickname (P2006 -> P06T)
        assert_eq!(top(&idx, "Asobo PassiveAircraft Generic Glider 2S18M"), "GLID"); // "glider" -> code GLID
        assert_eq!(top(&idx, "Asobo PassiveAircraft S2000"), "SB20"); // nickname (S2000 -> SB20)
        assert_eq!(top(&idx, "MXS-R"), "MXS"); // code token verbatim
        assert_eq!(top(&idx, "Savage Norden: Aerial Advertising"), "SAVG"); // distinctive model word
        assert_eq!(top(&idx, "Robin DR400"), "DR40"); // code DR40 is a prefix of "dr400"
        assert_eq!(top(&idx, "Asobo PassiveAircraft P2008"), "P208"); // model token "p2008"
        assert_eq!(top(&idx, "Asobo PassiveAircraft P92"), "ECHO"); // model token "p92"

        // Trailing-token noise doesn't dislodge a code/model match.
        assert_eq!(top(&idx, "MXS-R Race"), "MXS");

        // Add-on / variant / marketing shorthands carried by curated nicknames.
        assert_eq!(top(&idx, "Microsoft PassiveAircraft S340B Cargo"), "SF34");
        assert_eq!(top(&idx, "FSR500 VYA Old School"), "P46T");
        assert_eq!(top(&idx, "FSR500 VYA New School"), "P46T");
        assert_eq!(top(&idx, "mg hjet ha420 [Preset Default]"), "HDJT");
        assert_eq!(top(&idx, "CRJ550 Privat D-ALKI"), "CRJ7");
        assert_eq!(top(&idx, "FSLTL_B73X_ZZZZ"), "B737");
        assert_eq!(top(&idx, "FreedomFox"), "FOX");
        assert_eq!(top(&idx, "Powrachute Sky Rascal"), "PARA");
        // FlyByWire's developer shorthand resolves to its A320neo even with no type word.
        assert_eq!(top(&idx, "FWB JetBlue N4022J"), "A20N");

        // Distinctive model words.
        assert_eq!(top(&idx, "Virus SW Pipistrel Private Charter"), "PIVI");
        assert_eq!(top(&idx, "Taurus M: Passengers"), "PITA");
        assert_eq!(top(&idx, "Optica: Passengers"), "OPCA");
        assert_eq!(top(&idx, "Optica: Scientific Research"), "OPCA");
        assert_eq!(top(&idx, "Seastar"), "CD2");
        assert_eq!(top(&idx, "Tundra 29in"), "TNDR");
        // "Vertigo" is a Lancair Legacy edition name carried by a nickname (the model columns
        // say "Lancair Legacy", which this title doesn't mention).
        assert_eq!(top(&idx, "Vertigo: Inferno"), "LEG2");
        // A bare "Legacy" title is the Lancair, not the Embraer ERJ/bizjets that also list
        // the word; Embraer titles still win on their model number.
        assert_eq!(top(&idx, "LEGACY G3X GTN750 TURBO"), "LEG2");
        assert_eq!(top(&idx, "LEGACY ANALOG IFD5 TURBO"), "LEG2");
        assert_eq!(top(&idx, "LEGACY ANALOG GTN750"), "LEG2");
        assert_eq!(top(&idx, "Embraer Legacy 600"), "E35L");
        assert_eq!(top(&idx, "Embraer Legacy 450"), "E545");
        // No Fokker collides in the DB, so the brand word alone pins the trimotor.
        assert_eq!(top(&idx, "Fokker F-VIIa/3m Skis"), "F7");
        assert_eq!(top(&idx, "Fokker Replica Cargo"), "F7");
        assert_eq!(top(&idx, "Fokker Replica Passenger"), "F7");

        // Verbatim codes / code-as-prefix / model tokens for the remaining types.
        assert_eq!(top(&idx, "JMB Aviation VL3"), "VL3");
        assert_eq!(top(&idx, "Asobo PassiveAircraft C336"), "C336");
        assert_eq!(top(&idx, "Asobo PassiveAircraft M7"), "M7");
        assert_eq!(top(&idx, "Asobo PassiveAircraft M9"), "M9");
        assert_eq!(top(&idx, "Asobo PassiveAircraft P2012"), "P212");
        assert_eq!(top(&idx, "Asobo PassiveAircraft E140"), "E135"); // ERJ-140 shares the E135 code
        assert_eq!(top(&idx, "Edge540 v2"), "EDGE"); // code EDGE is a prefix of "edge540"
        assert_eq!(top(&idx, "Edge540 v3 Kirby Chambliss"), "EDGE");
        assert_eq!(top(&idx, "Edge540 v3 Bullet"), "EDGE");

        // Every Super Hornet title resolves to F18S — the spelled-out form via the type's own
        // "Super Hornet" model words (and the "super hornet" nickname), the glued "FA18E" form
        // via nickname. F18H is reserved for the plain F/A-18 Hornet.
        assert_eq!(top(&idx, "Asobo PassiveAircraft FA18E"), "F18S");
        assert_eq!(top(&idx, "FA18E SuperHornet"), "F18S");
        assert_eq!(top(&idx, "F/A-18E Super Hornet VFA-103"), "F18S");

        // Generic gliders: "Generic Glider" via the code-as-prefix of "glider", plus glider
        // model names carried by nicknames.
        assert_eq!(top(&idx, "Asobo PassiveAircraft Generic Glider 1S18M"), "GLID");
        assert_eq!(top(&idx, "Asobo PassiveAircraft Generic Glider 2S20M"), "GLID");
        assert_eq!(top(&idx, "Asobo PassiveAircraft DG1001E"), "GLID");
        assert_eq!(top(&idx, "DG LS8"), "GLID");
        assert_eq!(top(&idx, "Asobo PassiveAircraft Generic Glider 11S18M"), "GLID");
        assert_eq!(top(&idx, "Asobo PassiveAircraft Generic Glider 1S15M"), "GLID");

        // Codes/model words for distinct new types.
        assert_eq!(top(&idx, "L-39 Albatros"), "L39");
        assert_eq!(top(&idx, "Asobo PassiveAircraft DA20"), "DA20"); // distinct from DV20 (Katana)
        assert_eq!(top(&idx, "Asobo PassiveAircraft T182"), "C82S"); // Turbo Skylane, distinct from C182
        assert_eq!(top(&idx, "Asobo PassiveAircraft P2002"), "SIRA");
        assert_eq!(top(&idx, "Asobo PassiveAircraft P2004"), "BRAV");
        assert_eq!(top(&idx, "Asobo PassiveAircraft C337"), "C337"); // Skymaster, distinct from C336
        assert_eq!(top(&idx, "Asobo PassiveAircraft T50"), "T50");

        // Nicknames: AT-802 (existing AT8T) and the Skyship 600 airship.
        assert_eq!(top(&idx, "AT802 Aerial Application Sprayer"), "AT8T");
        assert_eq!(top(&idx, "Skyship600 Passenger"), "SKS6");

        // More FSLTL 737 and Edge 540 / S340B title variants.
        assert_eq!(top(&idx, "FSLTL_B73X_SNJ"), "B737");
        assert_eq!(top(&idx, "FSLTL_B73X_SKY"), "B737");
        assert_eq!(top(&idx, "Edge540 v3 Matt Hall"), "EDGE");
        assert_eq!(top(&idx, "Microsoft PassiveAircraft S340B Passenger"), "SF34");

        // More distinct types and variant codes.
        assert_eq!(top(&idx, "Magni M24 Plus White"), "MM24");
        assert_eq!(top(&idx, "Asobo PassiveAircraft C411"), "C411");
        assert_eq!(top(&idx, "Virus SW Pipistrel"), "PIVI");
        assert_eq!(top(&idx, "Asobo PassiveAircraft TR182"), "C82T"); // Turbo R182, distinct from C82R/C82S
        assert_eq!(top(&idx, "Asobo PassiveAircraft P2010"), "TWEN");
        assert_eq!(top(&idx, "Asobo PassiveAircraft C205"), "C205");
        assert_eq!(top(&idx, "HU16 Albatross Passengers"), "U16"); // "albatross", not L39's "Albatros"
        assert_eq!(top(&idx, "Joby [Preset Default]"), "JAS4");

        // Nicknames: E175 -> short-wing E75S (existing), CAP 10, Maule MT-7 -> M7, glider S12-G,
        // and another AT-802 livery.
        assert_eq!(top(&idx, "Asobo PassiveAircraft E175"), "E75S");
        assert_eq!(top(&idx, "Robin CAP10"), "CP10");
        assert_eq!(top(&idx, "Asobo PassiveAircraft MT7"), "M7");
        assert_eq!(top(&idx, "S12-G: Passengers"), "GLID");
        assert_eq!(top(&idx, "AT802 Firefighting"), "AT8T");
        assert_eq!(top(&idx, "Asobo PassiveAircraft Generic Glider 11S20M"), "GLID");
    }

    /// "TFDi Design" is a studio, but `design` is also the manufacturer of the
    /// Flight Design CT — which out-scored the actual model, so an MD-11F was
    /// logged as a light sport aircraft. Dropping the bare word would blind the
    /// index to the real manufacturer, so the studio is matched as a phrase.
    #[test]
    fn studio_names_made_of_real_words_do_not_hijack_the_type() {
        let idx = IcaoIndex::build(&test_db());
        assert_eq!(top(&idx, "TFDi Design MD-11F GE"), "MD11");
        assert_eq!(top(&idx, "TFDi Design MD-11F"), "MD11");
        // The manufacturer itself must still resolve.
        assert_eq!(top(&idx, "Flight Design CT"), "FDCT");
        assert_eq!(top(&idx, "Design"), "FDCT");
    }

    /// FSReborn liveries end in a studio code like "(S108)" — the Stinson 108's ICAO code —
    /// which out-scored the title's "Phenom 300E" when title and livery were one query.
    #[test]
    fn title_outweighs_livery() {
        let idx = IcaoIndex::build(&test_db());
        let top_tl = |t: &str, l: &str| idx.find_title_livery(t, l).map(|m| m.icao).unwrap_or_default();
        assert_eq!(top(&idx, "FSReborn Phenom 300E PRO Oberon Interior"), "E55P");
        assert_eq!(
            top_tl("FSReborn Phenom 300E PRO Oberon Interior", "Voyager Aviation | S2 (Dynamic) (S108)"),
            "E55P"
        );
        // A livery still decides when the title carries no type of its own.
        assert_eq!(top_tl("Asobo PassiveAircraft", "Boeing 737-800 American"), "B738");
    }

    #[test]
    fn ignores_addon_developer_names() {
        let idx = IcaoIndex::build(&test_db());
        // Add-on studio names in the title must not affect type resolution.
        assert_eq!(top(&idx, "Fenix Airbus A320"), "A320");
        assert_eq!(top(&idx, "iFly Boeing 737-800"), "B738");
        // A title made up only of studio names carries no type signal.
        assert!(idx.find("PassiveAircraft Laminar").is_none());
    }
}

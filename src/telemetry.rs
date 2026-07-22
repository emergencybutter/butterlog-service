//! Shared flight-telemetry field catalogue. The Discord embed (`discord.rs`) and
//! the web flight-detail page (`main.rs`) both pull the same per-flight snapshots
//! out of `flights.statistics`, so the field table and value formatting live here
//! and render identically in both places.

use serde_json::Value;

pub struct TelemetryField {
    pub key: &'static str,
    pub friendly_name: &'static str,
    pub unit: &'static str,
    pub digits: usize,
    pub categories: &'static [&'static str],
}

pub const TELEMETRY_FIELDS: &[TelemetryField] = &[
    TelemetryField { key: "AltB", friendly_name: "Altitude (Barometric)", unit: "ft", digits: 2, categories: &["normal", "max"] },
    TelemetryField { key: "BaroA", friendly_name: "Kohlsmann Setting", unit: "inHg", digits: 2, categories: &["instruments"] },
    TelemetryField { key: "AltGPS", friendly_name: "Altitude (GPS)", unit: "ft", digits: 2, categories: &["normal", "max"] },
    TelemetryField { key: "OAT", friendly_name: "Outside Air Temperature", unit: "C", digits: 2, categories: &["normal", "max"] },
    TelemetryField { key: "IAS", friendly_name: "Indicated Airspeed", unit: "kts", digits: 2, categories: &["normal", "max"] },
    TelemetryField { key: "TAS", friendly_name: "True Airspeed", unit: "kts", digits: 2, categories: &["normal", "max"] },
    TelemetryField { key: "GndSpd", friendly_name: "Ground Speed", unit: "kts", digits: 2, categories: &["normal", "max"] },
    TelemetryField { key: "VSpd", friendly_name: "Vertical Speed", unit: "fpm", digits: 2, categories: &["normal", "max", "landing"] },
    TelemetryField { key: "Pitch", friendly_name: "Pitch", unit: "deg", digits: 2, categories: &["landing"] },
    TelemetryField { key: "Roll", friendly_name: "Roll", unit: "deg", digits: 2, categories: &["landing"] },
    TelemetryField { key: "NormAc", friendly_name: "Normal Acceleration", unit: "G", digits: 2, categories: &["normal", "max", "landing"] },
    TelemetryField { key: "volt1", friendly_name: "Voltage 1", unit: "V", digits: 2, categories: &["engine"] },
    TelemetryField { key: "volt2", friendly_name: "Voltage 2", unit: "V", digits: 2, categories: &["engine"] },
    TelemetryField { key: "amp1", friendly_name: "Amperage 1", unit: "A", digits: 2, categories: &["engine"] },
    TelemetryField { key: "FQtyL", friendly_name: "Fuel Quantity Left", unit: "Gal", digits: 1, categories: &["engine"] },
    TelemetryField { key: "FQtyR", friendly_name: "Fuel Quantity Right", unit: "Gal", digits: 1, categories: &["engine"] },
    TelemetryField { key: "E1 FFlow", friendly_name: "Engine 1 Fuel Flow", unit: "Gal/h", digits: 2, categories: &["engine"] },
    TelemetryField { key: "E1 OilT", friendly_name: "Engine 1 Oil Temperature", unit: "F", digits: 2, categories: &["engine", "max"] },
    TelemetryField { key: "E1 OilP", friendly_name: "Engine 1 Oil Pressure", unit: "psi", digits: 2, categories: &["engine", "max"] },
    TelemetryField { key: "E1 MAP", friendly_name: "Engine 1 Manifold Pressure", unit: "inHg", digits: 2, categories: &["engine"] },
    TelemetryField { key: "E1 RPM", friendly_name: "Engine 1 RPM", unit: "rpm", digits: 2, categories: &["engine", "max"] },
    TelemetryField { key: "E1 %Pwr", friendly_name: "Engine 1 Percent Power", unit: "%", digits: 2, categories: &["engine"] },
    TelemetryField { key: "E1 CHT1", friendly_name: "Engine 1 Cylinder Head Temp 1", unit: "F", digits: 0, categories: &["engine", "max"] },
    TelemetryField { key: "E1 EGT1", friendly_name: "Engine 1 Exhaust Gas Temp 1", unit: "F", digits: 0, categories: &["engine", "max"] },
    TelemetryField { key: "E1 TIT1", friendly_name: "Engine 1 Turbine Inlet Temp 1", unit: "F", digits: 0, categories: &["engine"] },
    TelemetryField { key: "E1 TIT2", friendly_name: "Engine 1 Turbine Inlet Temp 2", unit: "F", digits: 0, categories: &["engine"] },
    TelemetryField { key: "COM1", friendly_name: "COM1", unit: "MHz", digits: 3, categories: &["instruments"] },
    TelemetryField { key: "COM2", friendly_name: "COM2", unit: "MHz", digits: 3, categories: &["instruments"] },
    TelemetryField { key: "WndSpd", friendly_name: "Wind Speed", unit: "kts", digits: 2, categories: &["normal", "max", "landing"] },
    TelemetryField { key: "WndDr", friendly_name: "Wind Direction", unit: "deg", digits: 2, categories: &["normal"] },
    TelemetryField { key: "AfcsOn", friendly_name: "Autopilot", unit: "", digits: 0, categories: &["instruments"] },
];

/// Format just the value portion of a field (e.g. `-700.50 fpm`, `On`, or a raw
/// string), without the field label. `None` when the value can't be rendered.
pub fn format_value(field: &TelemetryField, value: &Value) -> Option<String> {
    let with_unit = |val: f64| {
        let formatted = format!("{:.*}", field.digits, val);
        if field.unit.is_empty() { formatted } else { format!("{} {}", formatted, field.unit) }
    };
    match value {
        Value::Bool(b) => Some(if *b { "On" } else { "Off" }.to_string()),
        Value::Number(num) => {
            let val = num.as_f64()?;
            if field.key == "AfcsOn" {
                Some(if val > 0.5 { "On" } else { "Off" }.to_string())
            } else {
                Some(with_unit(val))
            }
        }
        Value::String(s) => {
            if let Ok(val) = s.parse::<f64>() {
                if field.key == "AfcsOn" {
                    Some(if val > 0.5 { "On" } else { "Off" }.to_string())
                } else {
                    Some(with_unit(val))
                }
            } else if s.eq_ignore_ascii_case("true") {
                Some("On".to_string())
            } else if s.eq_ignore_ascii_case("false") {
                Some("Off".to_string())
            } else {
                Some(s.clone())
            }
        }
        _ => None,
    }
}

/// `(friendly_name, formatted_value)` pairs for every field in `category` that is
/// present and non-null in `snapshot`, in table order.
pub fn labeled_values(snapshot: &Value, category: &str) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for field in TELEMETRY_FIELDS {
        if field.categories.contains(&category) {
            if let Some(val) = snapshot.get(field.key) {
                if !val.is_null() {
                    if let Some(v) = format_value(field, val) {
                        out.push((field.friendly_name, v));
                    }
                }
            }
        }
    }
    out
}

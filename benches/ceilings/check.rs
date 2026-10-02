//! The ceilings the benches are held to, apart from the benches so a test can check them
//! without compiling bhai again.

use std::collections::BTreeMap;

use serde_json::Value;

/// Every bench, as criterion names its directory: group, then function.
pub const BENCHES: [&str; 4] = [
    "render/transcript_cold_120x40",
    "render/transcript_tail_120x40",
    "wrap/line_1mib",
    "profile/build_10k",
];

pub const CEILINGS: &str = include_str!("ceilings.toml");

/// The ceilings file: bench id to the most its median may take, in microseconds. Every
/// bench has one and every one names a bench, so neither a new bench nor a renamed one
/// runs unchecked.
pub fn ceilings(text: &str) -> Result<BTreeMap<String, f64>, String> {
    let ceilings: BTreeMap<String, f64> = toml::from_str(text).map_err(|e| e.to_string())?;
    for id in BENCHES {
        match ceilings.get(id) {
            None => return Err(format!("no ceiling for {id}")),
            Some(&max) if max.is_nan() || max <= 0.0 => {
                return Err(format!("{id}: ceiling {max} is not above 0"));
            }
            Some(_) => {}
        }
    }
    if let Some(id) = ceilings.keys().find(|id| !BENCHES.contains(&id.as_str())) {
        return Err(format!("{id} names no bench"));
    }
    Ok(ceilings)
}

/// The median of a criterion `estimates.json`, which records nanoseconds, in microseconds.
pub fn median_micros(estimates: &str) -> Option<f64> {
    let value: Value = serde_json::from_str(estimates).ok()?;
    Some(value["median"]["point_estimate"].as_f64()? / 1_000.0)
}

/// The line to fail on when `median` is past `ceiling`.
pub fn breach(id: &str, median: f64, ceiling: f64) -> Option<String> {
    (median > ceiling)
        .then(|| format!("{id}: median {median:.1} µs is over its ceiling of {ceiling:.1} µs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(max: &str) -> String {
        BENCHES
            .iter()
            .map(|id| format!("\"{id}\" = {max}\n"))
            .collect()
    }

    #[test]
    fn the_checked_in_ceilings_cover_every_bench() {
        let ceilings = ceilings(CEILINGS).unwrap();
        assert_eq!(ceilings.len(), BENCHES.len());
    }

    #[test]
    fn a_missing_stray_or_empty_ceiling_is_refused() {
        assert!(ceilings(&all("100")).is_ok());
        let missing = all("100").replace("\"wrap/line_1mib\" = 100\n", "");
        assert_eq!(
            ceilings(&missing).unwrap_err(),
            "no ceiling for wrap/line_1mib"
        );
        let stray = all("100") + "\"wrap/gone\" = 5\n";
        assert_eq!(ceilings(&stray).unwrap_err(), "wrap/gone names no bench");
        assert!(ceilings(&all("0")).unwrap_err().contains("is not above 0"));
        assert!(ceilings(&all("nan")).is_err());
    }

    #[test]
    fn the_median_is_read_in_microseconds() {
        let estimates = r#"{"mean":{"point_estimate":9e9},"median":{"point_estimate":1500.0,
            "standard_error":3.0}}"#;
        assert_eq!(median_micros(estimates), Some(1.5));
        assert_eq!(median_micros(r#"{"mean":{"point_estimate":1.0}}"#), None);
        assert_eq!(median_micros("not json"), None);
    }

    #[test]
    fn only_a_median_past_the_ceiling_fails() {
        assert_eq!(breach("wrap/line_1mib", 100.0, 100.0), None);
        assert_eq!(
            breach("wrap/line_1mib", 100.4, 100.0).unwrap(),
            "wrap/line_1mib: median 100.4 µs is over its ceiling of 100.0 µs"
        );
    }
}

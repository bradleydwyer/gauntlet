use std::collections::HashMap;

use crate::schema::MatrixConfig;

/// A single combination of matrix dimension values.
pub type MatrixCombination = HashMap<String, String>;

/// Expand a matrix config into all combinations (cartesian product minus exclusions).
pub fn expand(config: &MatrixConfig) -> Vec<MatrixCombination> {
    if config.dimensions.is_empty() {
        return vec![];
    }

    // Sort dimension keys for deterministic ordering.
    let mut keys: Vec<&String> = config.dimensions.keys().collect();
    keys.sort();

    // Build cartesian product.
    let mut combos: Vec<MatrixCombination> = vec![HashMap::new()];
    for key in &keys {
        let values = &config.dimensions[*key];
        let mut next = Vec::with_capacity(combos.len() * values.len());
        for combo in &combos {
            for val in values {
                let mut c = combo.clone();
                c.insert((*key).clone(), val.clone());
                next.push(c);
            }
        }
        combos = next;
    }

    // Remove excluded combinations.
    combos.retain(|combo| {
        !config.exclude.iter().any(|excl| {
            excl.iter()
                .all(|(k, v)| combo.get(k).map(|cv| cv == v).unwrap_or(false))
        })
    });

    combos
}

/// Generate a task ID suffix from a matrix combination.
/// Deterministic: sorted by key, values joined with `-`.
pub fn suffix(combo: &MatrixCombination) -> String {
    let mut keys: Vec<&String> = combo.keys().collect();
    keys.sort();
    keys.iter()
        .map(|k| sanitize(&combo[*k]))
        .collect::<Vec<_>>()
        .join("-")
}

/// Sanitize a value for use in a task ID: lowercase, non-alphanumeric → `-`.
fn sanitize(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::MatrixConfig;

    #[test]
    fn single_dimension() {
        let config = MatrixConfig {
            dimensions: HashMap::from([(
                "toolchain".into(),
                vec!["stable".into(), "nightly".into()],
            )]),
            exclude: vec![],
        };
        let combos = expand(&config);
        assert_eq!(combos.len(), 2);
        // Order follows the input vec order, not alphabetical.
        assert_eq!(suffix(&combos[0]), "stable");
        assert_eq!(suffix(&combos[1]), "nightly");
    }

    #[test]
    fn two_dimensions() {
        let config = MatrixConfig {
            dimensions: HashMap::from([
                ("os".into(), vec!["linux".into(), "macos".into()]),
                ("rust".into(), vec!["stable".into(), "nightly".into()]),
            ]),
            exclude: vec![],
        };
        let combos = expand(&config);
        assert_eq!(combos.len(), 4);
    }

    #[test]
    fn with_exclusion() {
        let config = MatrixConfig {
            dimensions: HashMap::from([
                ("os".into(), vec!["linux".into(), "macos".into()]),
                ("rust".into(), vec!["stable".into(), "nightly".into()]),
            ]),
            exclude: vec![HashMap::from([
                ("os".into(), "macos".into()),
                ("rust".into(), "nightly".into()),
            ])],
        };
        let combos = expand(&config);
        assert_eq!(combos.len(), 3);
        // macos + nightly should be excluded
        assert!(
            !combos
                .iter()
                .any(|c| c["os"] == "macos" && c["rust"] == "nightly")
        );
    }

    #[test]
    fn empty_dimensions() {
        let config = MatrixConfig {
            dimensions: HashMap::new(),
            exclude: vec![],
        };
        assert!(expand(&config).is_empty());
    }

    #[test]
    fn suffix_sanitization() {
        let combo = HashMap::from([("ver".into(), "1.80.0".into())]);
        assert_eq!(suffix(&combo), "1-80-0");
    }
}

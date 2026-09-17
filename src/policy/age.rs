use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A duration written as `Ns`, `Nm`, `Nh` or `Nd`, kept in its source form
/// for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct Age {
    secs: u64,
    unit: char,
    count: u64,
}

impl Age {
    pub fn duration(self) -> Duration {
        Duration::from_secs(self.secs)
    }

    /// `d` in its largest whole unit, for a reason such as "published 2h ago".
    pub fn approx(d: Duration) -> Self {
        let secs = d.as_secs();
        let (unit, per_unit) = match secs {
            s if s >= 86_400 => ('d', 86_400),
            s if s >= 3600 => ('h', 3600),
            s if s >= 60 => ('m', 60),
            _ => ('s', 1),
        };
        let count = secs / per_unit;
        Self {
            secs: count * per_unit,
            unit,
            count,
        }
    }
}

impl FromStr for Age {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let err =
            || format!("invalid age '{s}': expected a whole number of s, m, h or d (e.g. 48h, 7d)");
        let (digits, unit) = s.split_at(s.len().saturating_sub(1));
        let unit = unit.chars().next().ok_or_else(err)?;
        let count: u64 = match digits {
            "" => return Err(err()),
            d if d.bytes().all(|b| b.is_ascii_digit()) => d.parse().map_err(|_| err())?,
            _ => return Err(err()),
        };
        let per_unit = match unit {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => return Err(err()),
        };
        let secs = count.checked_mul(per_unit).ok_or_else(err)?;
        Ok(Self { secs, unit, count })
    }
}

impl TryFrom<String> for Age {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        s.parse()
    }
}

impl From<Age> for String {
    fn from(a: Age) -> String {
        a.to_string()
    }
}

impl fmt::Display for Age {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.count, self.unit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_s_m_h_d() {
        for (text, secs) in [("30s", 30), ("10m", 600), ("48h", 172_800), ("7d", 604_800)] {
            assert_eq!(
                text.parse::<Age>().unwrap().duration().as_secs(),
                secs,
                "{text}"
            );
        }
        assert_eq!(" 1h ".parse::<Age>().unwrap().duration().as_secs(), 3600);
    }

    #[test]
    fn rejects_words_weeks_and_spaces() {
        for bad in [
            "7 days",
            "1w",
            "",
            "h",
            "-1h",
            "1.5h",
            "7",
            "1 h",
            "99999999999999999999d",
        ] {
            let err = bad.parse::<Age>().unwrap_err();
            assert!(err.contains("s, m, h or d"), "{bad}: {err}");
        }
    }

    #[test]
    fn display_roundtrips() {
        for text in ["48h", "7d", "30s", "10m"] {
            assert_eq!(text.parse::<Age>().unwrap().to_string(), text);
        }
        let json = serde_json::to_string(&"48h".parse::<Age>().unwrap()).unwrap();
        assert_eq!(json, "\"48h\"");
        assert_eq!(
            serde_json::from_str::<Age>(&json).unwrap(),
            "48h".parse().unwrap()
        );
        for (secs, text) in [(0, "0s"), (59, "59s"), (7_320, "2h"), (2_600_000, "30d")] {
            assert_eq!(Age::approx(Duration::from_secs(secs)).to_string(), text);
        }
    }

    #[test]
    fn config_rejects_bad_age() {
        let toml = r#"
[policy.npm-proxy]
min_release_age = "7 days"
"#;
        let err = toml::from_str::<crate::config::Config>(toml)
            .unwrap_err()
            .to_string();
        assert!(err.contains("min_release_age"), "{err}");
        assert!(err.contains("s, m, h or d"), "{err}");
        let ok: crate::config::Config = toml::from_str(
            r#"
[policy.npm-proxy]
min_release_age = "7d"
"#,
        )
        .unwrap();
        assert_eq!(
            ok.policy["npm-proxy"]
                .min_release_age
                .unwrap()
                .duration()
                .as_secs(),
            604_800
        );
    }
}

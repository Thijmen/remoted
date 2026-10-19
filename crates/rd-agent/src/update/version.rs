//! Release versions, `X.Y.Z` with an optional `-pre` part, ordered by semver precedence (as
//! tools/release/manifest.ts orders them).

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use anyhow::{Context as _, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    core: [u64; 3],
    /// Dot-separated pre-release identifiers; empty for a release.
    pre: Vec<String>,
}

impl FromStr for Version {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        // Build metadata (`+…`) does not take part in the order.
        let s = s.trim();
        let s = s.split_once('+').map_or(s, |(v, _)| v);
        let (core, pre) = match s.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (s, None),
        };
        let mut parts = core.split('.');
        let mut number = || -> anyhow::Result<u64> {
            let part = parts
                .next()
                .with_context(|| format!("{s:?} is not X.Y.Z"))?;
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                bail!("{s:?} is not X.Y.Z");
            }
            part.parse().with_context(|| format!("{s:?} is not X.Y.Z"))
        };
        let core = [number()?, number()?, number()?];
        if parts.next().is_some() {
            bail!("{s:?} is not X.Y.Z");
        }
        let pre = match pre {
            None => Vec::new(),
            Some(pre) => {
                let ids: Vec<String> = pre.split('.').map(str::to_owned).collect();
                if ids.iter().any(|id| {
                    id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                }) {
                    bail!("{s:?} has an invalid pre-release part");
                }
                ids
            }
        };
        Ok(Version { core, pre })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core.cmp(&other.core).then_with(|| {
            // A pre-release comes before its release.
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        let order = match (a.parse::<u64>(), b.parse::<u64>()) {
                            (Ok(a), Ok(b)) => a.cmp(&b),
                            // Numeric identifiers come before alphanumeric ones.
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => a.cmp(b),
                        };
                        if order != Ordering::Equal {
                            return order;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            }
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [x, y, z] = self.core;
        write!(f, "{x}.{y}.{z}")?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        s.parse().unwrap()
    }

    #[test]
    fn parses() {
        assert_eq!(v("1.4.2").to_string(), "1.4.2");
        assert_eq!(v(" 1.5.0-rc.1 ").to_string(), "1.5.0-rc.1");
        assert_eq!(v("1.5.0+build.7"), v("1.5.0"));
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "1.x.3",
            "v1.2.3",
            "1.2.3-",
            "1.2.3-a..b",
            "-1.2.3",
        ] {
            assert!(bad.parse::<Version>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn orders_like_semver() {
        let ordered = [
            "0.9.9",
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.4.2",
            "1.5.0",
            "1.10.0",
            "2.0.0",
        ];
        for pair in ordered.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
        assert_eq!(v("1.5.0").cmp(&v("1.5.0")), Ordering::Equal);
    }
}

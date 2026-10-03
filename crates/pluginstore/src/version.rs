//! Version comparison (Go `internal/pluginstore/version.go`).

use crate::registry::normalize_version;

/// Reports whether `latest` should be offered as an upgrade over `installed`. A leading
/// "v"/"V" is ignored on both sides. Versions are compared numerically when both are
/// dotted release numbers, so an installed version newer than the registry one is not
/// reported as an update; otherwise any difference counts as an update.
pub fn update_available(installed: &str, latest: &str) -> bool {
    let installed = normalize_version(installed);
    let latest = normalize_version(latest);
    if installed.is_empty() || latest.is_empty() || installed == latest {
        return false;
    }
    match compare_versions(&installed, &latest) {
        None => true,
        Some(ordering) => ordering.is_lt(),
    }
}

/// Compares dotted numeric versions segment by segment, with missing segments treated
/// as zero. Returns `None` when either version contains a non-numeric segment.
fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let segments_a: Vec<&str> = a.split('.').collect();
    let segments_b: Vec<&str> = b.split('.').collect();
    let length = segments_a.len().max(segments_b.len());
    for index in 0..length {
        let number_a = version_segment(&segments_a, index)?;
        let number_b = version_segment(&segments_b, index)?;
        if number_a != number_b {
            return Some(number_a.cmp(&number_b));
        }
    }
    Some(std::cmp::Ordering::Equal)
}

/// Go `strconv.ParseInt(seg, 10, 64)` with a non-negative requirement.
fn version_segment(segments: &[&str], index: usize) -> Option<i64> {
    let Some(segment) = segments.get(index) else {
        return Some(0);
    };
    let number: i64 = segment.parse().ok()?;
    (number >= 0).then_some(number)
}

#[cfg(test)]
mod tests {
    use super::update_available;

    #[test]
    fn update_available_cases() {
        let cases = [
            ("unknown installed", "", "0.2.0", false),
            ("same version", "0.1.0", "0.1.0", false),
            ("same version with v prefix", "v0.1.0", "0.1.0", false),
            ("newer registry version", "0.1.0", "0.2.0", true),
            ("newer registry version with v prefix", "v0.1.0", "0.2.0", true),
            ("numeric not lexicographic", "0.1.9", "0.1.10", true),
            ("installed newer than registry", "0.2.0", "0.1.0", false),
            ("missing segments treated as zero", "0.1", "0.1.0", false),
            ("prerelease falls back to inequality", "0.1.0-rc1", "0.1.0", true),
            ("non numeric falls back to inequality", "dev", "0.1.0", true),
        ];
        for (name, installed, latest, want) in cases {
            assert_eq!(update_available(installed, latest), want, "{name}");
        }
    }
}

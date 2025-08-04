use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::prelude::*;
use rpm::Evr;

use crate::model::*;
use crate::ostreeutil;

/// Parse the output of `rpm -q`
fn rpm_parse_metadata(stdout: &[u8]) -> Result<ContentMetadata> {
    let pkgs = std::str::from_utf8(stdout)?
        .split_whitespace()
        .map(|s| -> Result<_> {
            let parts: Vec<_> = s.splitn(2, ',').collect();
            let name = parts[0];
            if let Some(ts) = parts.get(1) {
                let nt = DateTime::parse_from_str(ts, "%s")
                    .context("Failed to parse rpm buildtime")?
                    .with_timezone(&chrono::Utc);
                Ok((name, nt))
            } else {
                bail!("Failed to parse: {}", s);
            }
        })
        .collect::<Result<BTreeMap<&str, DateTime<Utc>>>>()?;
    if pkgs.is_empty() {
        bail!("Failed to find any RPM packages matching files in source efidir");
    }
    let timestamps: BTreeSet<&DateTime<Utc>> = pkgs.values().collect();
    // Unwrap safety: We validated pkgs has at least one value above
    let largest_timestamp = timestamps.iter().last().unwrap();
    let version = pkgs.keys().fold("".to_string(), |mut s, n| {
        if !s.is_empty() {
            s.push(',');
        }
        s.push_str(n);
        s
    });
    Ok(ContentMetadata {
        timestamp: **largest_timestamp,
        version,
    })
}

/// Query the rpm database and list the package and build times.
pub(crate) fn query_files<T>(
    sysroot_path: &str,
    paths: impl IntoIterator<Item = T>,
) -> Result<ContentMetadata>
where
    T: AsRef<Path>,
{
    let mut c = ostreeutil::rpm_cmd(sysroot_path)?;
    c.args(["-q", "--queryformat", "%{nevra},%{buildtime} ", "-f"]);
    for arg in paths {
        c.arg(arg.as_ref());
    }

    let rpmout = c.output()?;
    if !rpmout.status.success() {
        std::io::stderr().write_all(&rpmout.stderr)?;
        bail!("Failed to invoke rpm -qf");
    }

    rpm_parse_metadata(&rpmout.stdout)
}

/// Compare two `Evr`s, ignoring the epoch if either is empty.
fn evr_cmp_ignore_epoch(a: &Evr, b: &Evr) -> Ordering {
    let (a_epoch, a_version, a_release) = (a.epoch(), a.version(), a.release());
    let (b_epoch, b_version, b_release) = (b.epoch(), b.version(), b.release());

    // If both epochs are non-empty and different, compare them
    if !a_epoch.is_empty() && !b_epoch.is_empty() {
        match a_epoch.cmp(b_epoch) {
            Ordering::Equal => {}
            non_eq => return non_eq,
        }
    }

    // Compare version
    match a_version.cmp(b_version) {
        Ordering::Equal => {}
        non_eq => return non_eq,
    }

    // Compare release
    a_release.cmp(b_release)
}

// Implement Compare package versions function:
// Return `Greater` if evr_a > evr_b,
// Return `Less` if evr_a < evr_b,
// Return `Equal` if evr_a == evr_b.
fn compare_package_versions_impl(a: &str, b: &str) -> Vec<(String, Ordering)> {
    fn parse_evr_map(input: &str) -> BTreeMap<String, Evr> {
        input
            .split(',')
            .filter_map(|pkg| {
                // assume it is like "grub2-2.12-28.fc42,shim-15.8-3"
                // epoch is null, using rpm::Evr to parse the str
                if !pkg.ends_with(std::env::consts::ARCH) {
                    let (name, version_release) = pkg.split_once('-').unwrap_or((pkg, ""));
                    let evr = Evr::parse(version_release);
                    return Some((name.to_string(), evr));
                }

                // assume it is like "grub2-efi-x64-1:2.12-28.fc42.x86_64,shim-x64-15.8-3.x86_64"
                // remove the architecture suffix (e.g., .x86_64)
                let ends = format!(".{}", std::env::consts::ARCH);
                let stripped = pkg.trim_end_matches(&ends);
                // Split release, like "grub2-efi-x64-1:2.12, 28.fc42"
                let (rest, release) = stripped.rsplit_once('-')?;

                // Split rest, like "grub2-efi-x64, 1:2.12"
                let (name_part, epoch_version) = rest.rsplit_once('-')?;

                // Check for epoch and version from "1:2.12"
                let (epoch, version) = if let Some((e, v)) = epoch_version.split_once(':') {
                    (e.to_string(), v.to_string())
                } else {
                    ("".to_string(), epoch_version.to_string()) // default epoch
                };

                // Get name "grub2" from "grub2-efi-x64"
                let name = name_part.split('-').next().unwrap();
                Some((
                    name.to_string(),
                    Evr::new(epoch, version, release.to_string()),
                ))
            })
            .collect()
    }

    let map_a = parse_evr_map(a);
    let map_b = parse_evr_map(b);

    // Compare package versions between `map_a` (current) and
    // `map_b` (target), ignoring epoch.
    // For each package in `map_b`:
    // - If it also exists in `map_a`, compare their versions and record the result.
    // - If it does not exist in `map_a`, assume it's a new package that should be upgraded.
    let mut result = Vec::new();
    for (name, evr_b) in map_b {
        if let Some(evr_a) = map_a.get(&name) {
            let cmp = evr_cmp_ignore_epoch(&evr_a, &evr_b);
            result.push((name, cmp));
        } else {
            log::trace!("Found new package {name} in the target");
            result.push((name, Ordering::Less));
        }
    }
    result
}

// Compare package versions:
// If any package is Ordering::Less, return Ordering::Less, means upgradable,
// Else if any package is Ordering::Greater, return Ordering::Greater,
// Else (all equal), return Ordering::Equal.
pub(crate) fn compare_package_versions(a: &str, b: &str) -> Ordering {
    // Fast path: if the two values are equal, skip detailed comparison
    if a == b {
        return Ordering::Equal;
    }
    let diffs = compare_package_versions_impl(a, b);

    let mut has_less = false;
    let mut has_greater = false;

    for (_, ord) in &diffs {
        match ord {
            Ordering::Less => {
                has_less = true;
            }
            Ordering::Greater => {
                has_greater = true;
            }
            Ordering::Equal => {}
        }
    }

    if has_less {
        Ordering::Less
    } else if has_greater {
        Ordering::Greater
    } else {
        Ordering::Equal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rpmout() {
        let testdata = "grub2-efi-x64-1:2.06-95.fc38.x86_64,1681321788 grub2-efi-x64-1:2.06-95.fc38.x86_64,1681321788 shim-x64-15.6-2.x86_64,1657222566 shim-x64-15.6-2.x86_64,1657222566 shim-x64-15.6-2.x86_64,1657222566";
        let parsed = rpm_parse_metadata(testdata.as_bytes()).unwrap();
        assert_eq!(
            parsed.version,
            "grub2-efi-x64-1:2.06-95.fc38.x86_64,shim-x64-15.6-2.x86_64"
        );
    }

    #[test]
    fn test_compare_package_versions() {
        let current = "grub2-efi-x64-1:2.12-28.fc42.x86_64,shim-x64-15.8-3.x86_64";
        let target = "grub2-efi-x64-1:2.12-29.fc42.x86_64,shim-x64-15.8-3.x86_64";
        let ord = compare_package_versions(current, target);
        assert_eq!(ord, Ordering::Less); // current < target

        let ord = compare_package_versions(target, current);
        assert_eq!(ord, Ordering::Greater);

        let current = "grub2-efi-x64-1:2.12-28.fc42.x86_64,shim-x64-15.8-3.x86_64";
        let target = "grub2-2.12-29.fc42,shim-15.8-3";
        let ord = compare_package_versions(current, target);
        assert_eq!(ord, Ordering::Less); // current < target

        let ord = compare_package_versions(target, current);
        assert_eq!(ord, Ordering::Greater);

        let current = "grub2-2.12-28.fc42,shim-15.8-3";
        let target = "grub2-2.12-28.fc42,shim-15.8-4";
        let ord = compare_package_versions(current, target);
        assert_eq!(ord, Ordering::Less); // current < target

        let ord = compare_package_versions(target, current);
        assert_eq!(ord, Ordering::Greater);

        // The target includes new package, should upgrade
        let current = "grub2-efi-x64-1:2.12-28.fc42.x86_64,shim-x64-15.8-3.x86_64";
        let target = "grub2-efi-x64-1:2.12-28.fc42.x86_64,shim-x64-15.8-3.x86_64,test";
        let ord = compare_package_versions(current, target);
        assert_eq!(ord, Ordering::Less);

        let ord = compare_package_versions(target, current);
        assert_eq!(ord, Ordering::Equal);

        // Not sure if this would happen
        // current_grub2 > target_grub2
        // current_shim < target_shim
        // In this case there is Ordering::Less, return Ordering::Less
        {
            let current = "grub2-2.12-28.fc42,shim-15.8-3";
            let target = "grub2-2.12-27.fc42,shim-15.8-4";
            let ord = compare_package_versions(current, target);
            assert_eq!(ord, Ordering::Less);

            let ord = compare_package_versions(target, current);
            assert_eq!(ord, Ordering::Less);
        }

        // Test Equal
        {
            let current = "grub2-efi-x64-1:2.12-28.fc42.x86_64,shim-x64-15.8-3.x86_64";
            let target = "grub2-2.12-28.fc42,shim-15.8-3";
            let ord = compare_package_versions(current, target);
            assert_eq!(ord, Ordering::Equal);

            let current = "grub2-2.12-28.fc42,shim-15.8-3";
            let target = "grub2-2.12-28.fc42,shim-15.8-3";
            let ord = compare_package_versions(current, target);
            assert_eq!(ord, Ordering::Equal);
        }

        // Test only grub2
        let current = "grub2-2.12-28.fc42";
        let target = "grub2-2.12-29.fc42";
        let ord = compare_package_versions(current, target);
        assert_eq!(ord, Ordering::Less);

        let ord = compare_package_versions(target, current);
        assert_eq!(ord, Ordering::Greater);
    }
}

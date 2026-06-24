//! CycloneDX-style SBOM assembly from a resolved `Cargo.lock`.
//!
//! [`Sbom::from_lockfile`] parses the lockfile offline — it never invokes
//! `cargo` or any network service — into one [`SbomComponent`] per locked
//! package, each carrying a package URL (purl). The SHA-256 hash of the
//! lockfile content is recorded so the document is traceable to a specific
//! resolution. A missing or unreadable lockfile yields a clear error rather
//! than a partial document. The emitted document follows the `CycloneDX`
//! interchange format.

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::SecurityError;

/// `CycloneDX` bom-format identifier emitted in the document.
const BOM_FORMAT: &str = "CycloneDX";
/// `CycloneDX` spec version targeted by the emitted document.
const SPEC_VERSION: &str = "1.5";

/// A single SBOM component derived from a locked package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SbomComponent {
    /// `CycloneDX` component type — always `"library"` for crate dependencies.
    #[serde(rename = "type")]
    pub component_type: String,
    /// Package name.
    pub name: String,
    /// Resolved version.
    pub version: String,
    /// Package URL identifying the component (`pkg:cargo/<name>@<version>`).
    pub purl: String,
}

/// A CycloneDX-style SBOM document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Sbom {
    /// `CycloneDX` bom format — `"CycloneDX"`.
    #[serde(rename = "bomFormat")]
    pub bom_format: String,
    /// `CycloneDX` spec version.
    #[serde(rename = "specVersion")]
    pub spec_version: String,
    /// SHA-256 hash of the source lockfile content, for traceability.
    pub lockfile_sha256: String,
    /// One component per locked package.
    pub components: Vec<SbomComponent>,
}

impl Sbom {
    /// Assembles an [`Sbom`] from the `Cargo.lock` at `lockfile_path`.
    ///
    /// # Errors
    ///
    /// Returns [`SecurityError::Io`] when the lockfile is missing or unreadable,
    /// or [`SecurityError::Lockfile`] when its contents are not valid lockfile
    /// TOML.
    pub fn from_lockfile(lockfile_path: &Path) -> Result<Self, SecurityError> {
        let content = std::fs::read_to_string(lockfile_path)?;
        Self::from_lockfile_str(&content)
    }

    /// Assembles an [`Sbom`] from raw `Cargo.lock` content.
    ///
    /// # Errors
    ///
    /// Returns [`SecurityError::Lockfile`] when `content` is not valid lockfile
    /// TOML.
    pub fn from_lockfile_str(content: &str) -> Result<Self, SecurityError> {
        let lock: Lockfile =
            toml::from_str(content).map_err(|e| SecurityError::Lockfile(e.to_string()))?;

        let components = lock
            .package
            .into_iter()
            .map(|pkg| {
                let purl = format!("pkg:cargo/{}@{}", pkg.name, pkg.version);
                SbomComponent {
                    component_type: "library".to_owned(),
                    name: pkg.name,
                    version: pkg.version,
                    purl,
                }
            })
            .collect();

        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        let lockfile_sha256 = hex_encode(&hasher.finalize());

        Ok(Self {
            bom_format: BOM_FORMAT.to_owned(),
            spec_version: SPEC_VERSION.to_owned(),
            lockfile_sha256,
            components,
        })
    }

    /// Serialises the SBOM to a pretty-printed JSON document.
    ///
    /// # Errors
    ///
    /// Returns [`SecurityError::Lockfile`] if JSON serialisation fails (which
    /// should not occur for this fixed shape).
    pub fn to_json(&self) -> Result<String, SecurityError> {
        serde_json::to_string_pretty(self).map_err(|e| SecurityError::Lockfile(e.to_string()))
    }
}

/// Lowercase hex-encodes a byte slice.
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// Minimal `Cargo.lock` shape — only the package list is needed.
#[derive(Debug, Deserialize)]
struct Lockfile {
    #[serde(default)]
    package: Vec<LockPackage>,
}

/// A single `[[package]]` entry.
#[derive(Debug, Deserialize)]
struct LockPackage {
    name: String,
    version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_LOCK: &str = r#"
version = 3

[[package]]
name = "alpha"
version = "1.2.3"

[[package]]
name = "beta"
version = "0.4.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;

    #[test]
    fn components_are_derived_from_lockfile() {
        let sbom = Sbom::from_lockfile_str(FIXTURE_LOCK).unwrap();
        assert_eq!(sbom.components.len(), 2);

        let alpha = &sbom.components[0];
        assert_eq!(alpha.name, "alpha");
        assert_eq!(alpha.version, "1.2.3");
        assert_eq!(alpha.purl, "pkg:cargo/alpha@1.2.3");
        assert_eq!(alpha.component_type, "library");
    }

    #[test]
    fn lockfile_hash_is_recorded() {
        let sbom = Sbom::from_lockfile_str(FIXTURE_LOCK).unwrap();
        assert_eq!(sbom.lockfile_sha256.len(), 64, "SHA-256 hex is 64 chars");
        // Deterministic for fixed content.
        let again = Sbom::from_lockfile_str(FIXTURE_LOCK).unwrap();
        assert_eq!(sbom.lockfile_sha256, again.lockfile_sha256);
    }

    #[test]
    fn document_carries_cyclonedx_metadata() {
        let sbom = Sbom::from_lockfile_str(FIXTURE_LOCK).unwrap();
        assert_eq!(sbom.bom_format, "CycloneDX");
        let json = sbom.to_json().unwrap();
        assert!(json.contains("\"bomFormat\": \"CycloneDX\""));
        assert!(json.contains("pkg:cargo/alpha@1.2.3"));
    }

    #[test]
    fn missing_lockfile_is_a_clear_error() {
        let err = Sbom::from_lockfile(Path::new("/no/such/Cargo.lock")).unwrap_err();
        assert!(matches!(err, SecurityError::Io(_)), "got: {err:?}");
    }

    #[test]
    fn unparseable_lockfile_is_a_clear_error() {
        let err = Sbom::from_lockfile_str("this is not = valid toml [[[").unwrap_err();
        assert!(matches!(err, SecurityError::Lockfile(_)), "got: {err:?}");
    }
}

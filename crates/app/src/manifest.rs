// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `tangible manifest`: inspect and validate artifact manifests
//! (MANIFEST_SPEC.md).
//!
//! `show` prints an artifact's stored manifest exactly as the library holds
//! it. `validate` reads a manifest file, from a backup, an export or another
//! deployment, through the same parser and consistency rules the server
//! uses, and says whether it is in the canonical form the server writes,
//! which is what lets two copies be compared byte for byte. Neither changes
//! anything.

use std::io::{Read as _, Write as _};
use std::path::Path;

use anyhow::{Context, Result};
use tangible_api::diagnostics::bytes;
use tangible_domain::ArtifactManifest;
use tangible_domain::manifest::ManifestParseError;
use tangible_storage::{FilesystemStore, ManifestStore};

use crate::config::{ManifestShowConfig, ManifestValidateConfig};

/// The largest manifest file `validate` reads. A real one is kilobytes; a
/// directory tree of thousands of files, a few megabytes.
const MAX_MANIFEST_BYTES: u64 = 64 << 20;

/// Print an artifact's stored manifest.
pub async fn show_command(config: &ManifestShowConfig) -> Result<()> {
    let objects = FilesystemStore::open(&config.storage_root)
        .await
        .with_context(|| {
            format!(
                "opening the library root at {}",
                config.storage_root.display()
            )
        })?;
    let manifests = ManifestStore::open(objects)
        .await
        .context("opening the manifest store")?;
    let json = shown(&manifests, config.artifact).await?;
    let mut out = std::io::stdout().lock();
    writeln!(out, "{json}")?;
    out.flush()?;
    Ok(())
}

/// An artifact's stored manifest, in canonical form.
async fn shown(manifests: &ManifestStore, artifact: tangible_domain::ArtifactId) -> Result<String> {
    let manifest = manifests
        .read(artifact)
        .await
        .with_context(|| format!("reading the manifest of {artifact}"))?;
    manifest.to_json().context("writing the manifest")
}

/// Validate a manifest file, or standard input for `-`.
pub fn validate_command(config: &ManifestValidateConfig) -> Result<()> {
    let text = read_bounded(&config.file)?;
    let verdict = validate(&text);
    let mut out = std::io::stdout().lock();
    for line in &verdict.lines {
        writeln!(out, "{line}")?;
    }
    out.flush()?;
    if !verdict.valid {
        anyhow::bail!("the manifest is not valid");
    }
    Ok(())
}

fn read_bounded(file: &Path) -> Result<String> {
    let mut text = String::new();
    if file.as_os_str() == "-" {
        std::io::stdin()
            .take(MAX_MANIFEST_BYTES + 1)
            .read_to_string(&mut text)
            .context("reading standard input")?;
    } else {
        std::fs::File::open(file)
            .with_context(|| format!("opening {}", file.display()))?
            .take(MAX_MANIFEST_BYTES + 1)
            .read_to_string(&mut text)
            .with_context(|| format!("reading {} as UTF-8 text", file.display()))?;
    }
    if text.len() as u64 > MAX_MANIFEST_BYTES {
        anyhow::bail!("the file is larger than the {MAX_MANIFEST_BYTES} bytes a manifest may be");
    }
    Ok(text)
}

/// What validating a manifest found.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Verdict {
    valid: bool,
    lines: Vec<String>,
}

/// Validate manifest text: parse it as the server does, then compare it with
/// the canonical form.
fn validate(text: &str) -> Verdict {
    let manifest = match ArtifactManifest::from_json(text) {
        Ok(manifest) => manifest,
        Err(error) => {
            let mut lines = vec![format!("invalid: {error}")];
            let cause = match &error {
                ManifestParseError::Decode(cause) => cause.to_string(),
                ManifestParseError::Invalid(cause) => cause.to_string(),
            };
            lines.push(format!("  {cause}"));
            return Verdict {
                valid: false,
                lines,
            };
        }
    };
    let mut lines = vec![
        format!("valid: artifact {}", manifest.artifact_id),
        format!("  schema {}", manifest.schema),
        format!(
            "  {} ({}), {} file(s), {}",
            manifest.classification.format,
            manifest.classification.artifact_kind,
            manifest.components.len(),
            bytes(manifest.classification.total_bytes)
        ),
        format!("  validation {}", manifest.validation.state),
    ];
    match manifest.to_json() {
        Ok(canonical) if canonical == text || text.strip_suffix('\n') == Some(&canonical) => {
            lines.push("  canonical: the bytes the server writes".to_owned());
        }
        Ok(_) => lines.push(
            "  not canonical: the same manifest, written differently (spacing, key order or \
             number form); `tangible manifest show` prints the canonical form"
                .to_owned(),
        ),
        Err(error) => lines.push(format!("  could not be written back: {error}")),
    }
    Verdict { valid: true, lines }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A well-formed manifest the fuzz seeds keep: one ISO file.
    fn manifest_seed() -> String {
        std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fuzz/seeds/manifest/iso.json"
        ))
        .unwrap()
    }

    #[test]
    fn a_valid_manifest_is_described_and_its_canonical_form_recognised() {
        let text = manifest_seed();
        let manifest = ArtifactManifest::from_json(&text).unwrap();
        let canonical = manifest.to_json().unwrap();

        let verdict = validate(&canonical);
        assert!(verdict.valid);
        assert!(
            verdict.lines[0].starts_with("valid: artifact "),
            "{verdict:?}"
        );
        assert!(
            verdict
                .lines
                .iter()
                .any(|line| line.contains("canonical: the bytes"))
        );
        assert!(
            validate(&format!("{canonical}\n"))
                .lines
                .iter()
                .any(|l| l.contains("canonical: the bytes"))
        );

        let compact =
            serde_json::to_string(&serde_json::from_str::<serde_json::Value>(&canonical).unwrap())
                .unwrap();
        let verdict = validate(&compact);
        assert!(verdict.valid, "{verdict:?}");
        assert!(
            verdict
                .lines
                .iter()
                .any(|line| line.contains("not canonical")),
            "{verdict:?}"
        );
    }

    #[test]
    fn an_invalid_manifest_says_why() {
        let verdict = validate("{\"schema\": \"tangible.manifest/v1alpha1\"}");
        assert!(!verdict.valid);
        assert!(verdict.lines[0].starts_with("invalid: manifest could not be decoded"));
        assert_eq!(verdict.lines.len(), 2);

        let verdict = validate("not json");
        assert!(!verdict.valid);
    }

    #[tokio::test]
    async fn show_prints_the_stored_manifest_in_canonical_form() {
        let root = std::env::temp_dir().join(format!(
            "tangible-manifest-show-{}",
            uuid::Uuid::now_v7().simple()
        ));
        let objects = FilesystemStore::open(&root).await.unwrap();
        let manifests = ManifestStore::open(objects.clone()).await.unwrap();
        let bytes = vec![7_u8; 204_800];
        let stored = objects
            .put_stream(
                &bytes[..],
                tangible_storage::IngestLimits {
                    max_bytes: None,
                    fsync: false,
                },
            )
            .await
            .unwrap();
        let hex = stored.digest.to_hex();
        // The seed, naming the object just stored.
        let text = manifest_seed()
            .replace(
                &format!("sha256/aa/aa/{}", "a".repeat(64)),
                &format!("sha256/{}/{}/{hex}", &hex[..2], &hex[2..4]),
            )
            .replace(&"a".repeat(64), &hex);
        let manifest = ArtifactManifest::from_json(&text).unwrap();
        manifests.write(&manifest).await.unwrap();

        let shown = shown(&manifests, manifest.artifact_id).await.unwrap();
        assert_eq!(shown, manifest.to_json().unwrap());
        assert!(
            validate(&shown)
                .lines
                .iter()
                .any(|l| l.contains("canonical: the bytes"))
        );
        let missing = super::shown(&manifests, tangible_domain::ArtifactId::generate()).await;
        assert!(missing.is_err());
        // Objects are stored read-only; make the tree removable again.
        let _ = std::process::Command::new("chmod")
            .arg("-R")
            .arg("u+w")
            .arg(&root)
            .status();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_file_past_the_bound_is_not_read() {
        let path = std::env::temp_dir().join(format!(
            "tangible-manifest-bound-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_MANIFEST_BYTES + 1).unwrap();
        let error = read_bounded(&path).unwrap_err().to_string();
        std::fs::remove_file(&path).unwrap();
        assert!(error.contains("larger than"), "{error}");
    }
}

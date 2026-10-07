// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Making one image from another.
//!
//! A [`DerivationEngine`] reads a parent artifact's files, already copied
//! into a staging directory at their logical paths, and writes one output
//! file. It never sees the content-addressed store: the caller stages the
//! input and promotes the output, so no tool ever runs against a canonical
//! object.
//!
//! The engine also says what the output preserved, as a [`LossCharacter`],
//! because a derivative that cannot be shown to hold the parent's bytes must
//! not be recorded as if it did.

use std::path::Path;

use async_trait::async_trait;
use tangible_domain::derivation::ChdOptions;
use tangible_domain::{LogicalPath, LossCharacter, Transformation};

/// What an engine is asked to do.
#[derive(Debug, Clone, Copy)]
pub struct DeriveRequest<'a> {
    /// The transformation.
    pub transformation: Transformation,
    /// Its options, already validated against it.
    pub options: &'a ChdOptions,
    /// A directory holding the parent's components at their logical paths.
    pub input_dir: &'a Path,
    /// The component the tool reads: the sheet or TOC for a CD layout, the
    /// image itself for an ISO.
    pub input: &'a LogicalPath,
    /// Where to write the result. It does not exist yet.
    pub output: &'a Path,
}

/// What an engine did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeriveReport {
    /// What the output preserved of the parent, as the engine established it.
    pub loss_character: LossCharacter,
    /// Observations worth keeping with the derivative.
    pub notes: Vec<String>,
}

/// Why an engine did not produce a derivative.
#[derive(Debug, thiserror::Error)]
pub enum DeriveError {
    /// The tool could not be run at all.
    #[error("the {tool} tool is not available: {detail}")]
    Unavailable {
        /// The tool.
        tool: &'static str,
        /// Why.
        detail: String,
    },
    /// The tool ran and refused or failed.
    #[error("{code}: {detail}")]
    Failed {
        /// A stable code.
        code: &'static str,
        /// What happened, from the tool's own output where it said.
        detail: String,
        /// Whether running it again could succeed.
        retryable: bool,
    },
    /// Reading or writing a file failed.
    #[error("{operation}: {source}")]
    Io {
        /// What was being done.
        operation: &'static str,
        /// The error.
        #[source]
        source: std::io::Error,
    },
}

impl DeriveError {
    /// Whether the same request could succeed later.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::Unavailable { .. } | Self::Io { .. } => true,
            Self::Failed { retryable, .. } => *retryable,
        }
    }

    /// The stable code for the job record.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Unavailable { .. } => "DERIVATION_TOOL_UNAVAILABLE",
            Self::Failed { code, .. } => code,
            Self::Io { .. } => "DERIVATION_IO_FAILED",
        }
    }
}

/// Something that makes derivatives.
#[async_trait]
pub trait DerivationEngine: Send + Sync {
    /// The tool's name, as lineage records it.
    fn tool_name(&self) -> &'static str;

    /// The tool's version, as lineage and the fingerprint record it.
    fn tool_version(&self) -> String;

    /// Whether this engine can run a transformation, and why not. Asked
    /// before a job is queued, so an operator hears it at once rather than
    /// from a failed job.
    ///
    /// # Errors
    ///
    /// Why it cannot, in words an operator can act on.
    fn check_supported(&self, transformation: Transformation) -> Result<(), String> {
        let _ = transformation;
        Ok(())
    }

    /// Make the derivative.
    ///
    /// # Errors
    ///
    /// [`DeriveError`] when no output was produced. An engine that fails
    /// leaves nothing at `request.output` that a caller could mistake for a
    /// result.
    async fn derive(&self, request: &DeriveRequest<'_>) -> Result<DeriveReport, DeriveError>;
}

/// An engine that runs no tool, for tests and for a server with nothing
/// better configured.
///
/// Its output is a small file naming the transformation and every input
/// file's length, in a fixed order: deterministic, so two runs agree, and
/// different for different inputs, so a test can tell parents apart. It
/// claims nothing about fidelity: its loss character is `unknown` unless a
/// test says otherwise.
#[derive(Debug, Clone)]
pub struct FakeDeriver {
    loss_character: LossCharacter,
    fail_with: Option<(&'static str, bool)>,
    unsupported: Option<Transformation>,
}

impl Default for FakeDeriver {
    fn default() -> Self {
        Self {
            loss_character: LossCharacter::Unknown,
            fail_with: None,
            unsupported: None,
        }
    }
}

impl FakeDeriver {
    /// The version every fake derivative records.
    pub const VERSION: &'static str = "fake-1";

    /// Report this loss character.
    #[must_use]
    pub const fn reporting(mut self, loss_character: LossCharacter) -> Self {
        self.loss_character = loss_character;
        self
    }

    /// Say this transformation is not supported, as a tool too old to run it
    /// would.
    #[must_use]
    pub const fn refusing(mut self, transformation: Transformation) -> Self {
        self.unsupported = Some(transformation);
        self
    }

    /// Fail every request with this code, retryable or not.
    #[must_use]
    pub const fn failing(mut self, code: &'static str, retryable: bool) -> Self {
        self.fail_with = Some((code, retryable));
        self
    }
}

#[async_trait]
impl DerivationEngine for FakeDeriver {
    fn tool_name(&self) -> &'static str {
        "fake"
    }

    fn tool_version(&self) -> String {
        Self::VERSION.to_owned()
    }

    fn check_supported(&self, transformation: Transformation) -> Result<(), String> {
        if self.unsupported == Some(transformation) {
            Err(format!(
                "the fake engine was told not to run {transformation}"
            ))
        } else {
            Ok(())
        }
    }

    async fn derive(&self, request: &DeriveRequest<'_>) -> Result<DeriveReport, DeriveError> {
        if let Some((code, retryable)) = self.fail_with {
            return Err(DeriveError::Failed {
                code,
                detail: "the fake engine was told to fail".to_owned(),
                retryable,
            });
        }
        let io = |operation: &'static str| move |source| DeriveError::Io { operation, source };
        let input = request.input_dir.join(request.input.as_str());
        tokio::fs::metadata(&input)
            .await
            .map_err(io("reading the input"))?;

        let mut entries = Vec::new();
        let mut pending = vec![request.input_dir.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let mut read = tokio::fs::read_dir(&dir)
                .await
                .map_err(io("listing the input"))?;
            while let Some(entry) = read.next_entry().await.map_err(io("listing the input"))? {
                let meta = entry.metadata().await.map_err(io("reading the input"))?;
                if meta.is_dir() {
                    pending.push(entry.path());
                } else {
                    let relative = entry
                        .path()
                        .strip_prefix(request.input_dir)
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    entries.push(format!("{relative} {}", meta.len()));
                }
            }
        }
        entries.sort();
        let body = format!(
            "FAKE-DERIVATIVE {}\n{}\n",
            request.transformation,
            entries.join("\n")
        );
        tokio::fs::write(request.output, body)
            .await
            .map_err(io("writing the output"))?;
        Ok(DeriveReport {
            loss_character: self.loss_character,
            notes: vec!["made by the fake engine; no tool ran".to_owned()],
        })
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_fake_engine_is_deterministic_and_input_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        let input_dir = dir.path().join("in");
        std::fs::create_dir_all(&input_dir).unwrap();
        std::fs::write(input_dir.join("disc.cue"), b"FILE \"disc.bin\" BINARY\n").unwrap();
        std::fs::write(input_dir.join("disc.bin"), vec![0_u8; 2352]).unwrap();
        let options = ChdOptions::defaults(Transformation::ChdCreateCd);
        let input = LogicalPath::parse("disc.cue").unwrap();
        let run = |name: &str| {
            let output = dir.path().join(name);
            (output.clone(), output)
        };

        let (first, first_path) = run("a.chd");
        let report = FakeDeriver::default()
            .derive(&DeriveRequest {
                transformation: Transformation::ChdCreateCd,
                options: &options,
                input_dir: &input_dir,
                input: &input,
                output: &first,
            })
            .await
            .unwrap();
        assert_eq!(report.loss_character, LossCharacter::Unknown);
        let (second, second_path) = run("b.chd");
        FakeDeriver::default()
            .derive(&DeriveRequest {
                transformation: Transformation::ChdCreateCd,
                options: &options,
                input_dir: &input_dir,
                input: &input,
                output: &second,
            })
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(&first_path).unwrap(),
            std::fs::read(&second_path).unwrap()
        );

        std::fs::write(input_dir.join("disc.bin"), vec![0_u8; 4704]).unwrap();
        let (third, third_path) = run("c.chd");
        FakeDeriver::default()
            .derive(&DeriveRequest {
                transformation: Transformation::ChdCreateCd,
                options: &options,
                input_dir: &input_dir,
                input: &input,
                output: &third,
            })
            .await
            .unwrap();
        assert_ne!(
            std::fs::read(&first_path).unwrap(),
            std::fs::read(&third_path).unwrap()
        );
    }

    #[tokio::test]
    async fn a_failing_fake_engine_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let options = ChdOptions::defaults(Transformation::ChdCreateCd);
        let input = LogicalPath::parse("disc.cue").unwrap();
        let output = dir.path().join("out.chd");
        let error = FakeDeriver::default()
            .failing("FAKE_REFUSED", false)
            .derive(&DeriveRequest {
                transformation: Transformation::ChdCreateCd,
                options: &options,
                input_dir: dir.path(),
                input: &input,
                output: &output,
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), "FAKE_REFUSED");
        assert!(!error.is_retryable());
        assert!(!output.exists());
    }
}

// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Issue reports: where they go, and which build they describe.
//!
//! An operator can turn a burn attempt, an error or a diagnostics summary
//! into a prefilled GitHub issue. The server never sends one: it prepares
//! scrubbed text (`tangible_domain::report`), and the operator's browser
//! opens the issue form after they have read it. This module holds what the
//! server is configured with for that: the address issues go to, whether to
//! scrub addresses, and the build a report should name.

use std::collections::BTreeMap;

use serde::Serialize;
use tangible_domain::report::{Removed, ScrubContext, scrub};
use utoipa::ToSchema;

/// Where issues go unless configured otherwise: this project's tracker.
pub const DEFAULT_ISSUES_URL: &str = "https://github.com/digital-grease/tangible/issues/new";

/// The longest issue address accepted.
const MAX_ISSUES_URL_BYTES: usize = 512;

/// The longest release or revision kept from the build.
const MAX_BUILD_FIELD_CHARS: usize = 64;

/// Which build is running.
///
/// The version is the binary's own. The release and revision are what the
/// image was built from, passed in by the Dockerfile; a local build has
/// neither, and says so rather than guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildIdentity {
    /// The crate version compiled in, such as `0.1.0`.
    pub version: String,
    /// The release tag the image was built from, such as `v0.1.0`.
    pub release: Option<String>,
    /// The commit the image was built from.
    pub revision: Option<String>,
}

impl BuildIdentity {
    /// This binary, with what the build said about itself.
    ///
    /// `release` and `revision` come from the image (`TANGIBLE_BUILD_VERSION`
    /// and `TANGIBLE_BUILD_REVISION`). The Dockerfile's placeholders for a
    /// local build, `dev` and `unknown`, count as absent, and anything that
    /// is not a plain tag or commit is ignored rather than repeated into a
    /// report.
    #[must_use]
    pub fn current(release: Option<&str>, revision: Option<&str>) -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            release: build_field(release, "dev"),
            revision: build_field(revision, "unknown"),
        }
    }

    /// How a report names this build: `v0.1.0 (76fa515)` for a release
    /// image, `0.1.0, built locally` otherwise.
    #[must_use]
    pub fn describe(&self) -> String {
        let short = self
            .revision
            .as_deref()
            .map(|revision| revision.chars().take(7).collect::<String>());
        match (&self.release, short) {
            (Some(release), Some(short)) => format!("{release} ({short})"),
            (Some(release), None) => release.clone(),
            (None, Some(short)) => format!("{} ({short}), built locally", self.version),
            (None, None) => format!("{}, built locally", self.version),
        }
    }
}

/// A build field, if it is set, is not the local-build placeholder, and
/// looks like a tag or a commit.
fn build_field(value: Option<&str>, placeholder: &str) -> Option<String> {
    let value = value?.trim();
    let plain = !value.is_empty()
        && value != placeholder
        && value.chars().count() <= MAX_BUILD_FIELD_CHARS
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'));
    plain.then(|| value.to_owned())
}

/// One field of an issue form, filled in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct IssueReportField {
    /// The form's field id, which the prefilled link names.
    pub id: String,
    /// What the form calls the field.
    pub label: String,
    /// Its text, already scrubbed. Empty means left for the person.
    pub value: String,
}

/// An issue report, prepared for the operator to read and send.
///
/// Nothing in it has been sent anywhere. Every value is already in the
/// exact form the template expects, and free text has been scrubbed of
/// credentials, paths, serial numbers, library names and (by default)
/// addresses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct IssueReport {
    /// The issue template's file name, such as `burn_report.yml`.
    pub template: String,
    /// A title to suggest.
    pub title: Option<String>,
    /// The form's fields, in order.
    pub fields: Vec<IssueReportField>,
    /// The field that may be shortened to fit a link: the log, if any.
    pub long_field: Option<String>,
    /// What the scrubber replaced, by kind (`path`, `credential`, `name`,
    /// `serial`, `address`, `host`, `header_value`, `url_credentials`).
    pub removed: BTreeMap<String, usize>,
    /// Whether some text was too long and was cut short.
    pub truncated: bool,
}

/// Collects a report's fields, scrubbing the ones that need it and keeping
/// count of what was replaced. Generated values (a build, a dropdown option)
/// go in plain; anything that came from a log, a tool or a person is
/// scrubbed.
pub(crate) struct Writer {
    context: ScrubContext,
    fields: Vec<IssueReportField>,
    removed: BTreeMap<Removed, usize>,
    truncated: bool,
}

impl Writer {
    pub(crate) fn new(context: ScrubContext) -> Self {
        Self {
            context,
            fields: Vec::new(),
            removed: BTreeMap::new(),
            truncated: false,
        }
    }

    pub(crate) fn plain(&mut self, id: &str, label: &str, value: &str) {
        self.fields.push(IssueReportField {
            id: id.to_owned(),
            label: label.to_owned(),
            value: value.to_owned(),
        });
    }

    pub(crate) fn scrubbed(&mut self, id: &str, label: &str, text: &str) {
        let scrubbed = scrub(text, &self.context);
        for (kind, count) in scrubbed.removed {
            *self.removed.entry(kind).or_insert(0) += count;
        }
        self.truncated |= scrubbed.truncated;
        self.plain(id, label, &scrubbed.text);
    }

    pub(crate) fn finish(
        self,
        template: &str,
        title: Option<String>,
        long_field: Option<&str>,
    ) -> IssueReport {
        IssueReport {
            template: template.to_owned(),
            title,
            fields: self.fields,
            long_field: long_field.map(str::to_owned),
            removed: self
                .removed
                .into_iter()
                .map(|(kind, count)| (removed_name(kind).to_owned(), count))
                .collect(),
            truncated: self.truncated,
        }
    }
}

/// A kind of removal as the API names it.
fn removed_name(kind: Removed) -> &'static str {
    match kind {
        Removed::Credential => "credential",
        Removed::HeaderValue => "header_value",
        Removed::UrlCredentials => "url_credentials",
        Removed::Path => "path",
        Removed::Name => "name",
        Removed::Serial => "serial",
        Removed::Address => "address",
        Removed::Host => "host",
    }
}

/// The issue forms, read for the tests that check a report fits its form.
#[cfg(test)]
#[allow(clippy::panic)]
pub(crate) mod forms {
    use std::collections::BTreeMap;

    use super::IssueReport;

    /// A form's field ids, and each dropdown's options.
    pub(crate) type Form = BTreeMap<String, Option<Vec<String>>>;

    /// A form read from its YAML closely enough for these files: `id:` lines,
    /// and the `- ` items under an `options:` line.
    pub(crate) fn template(yaml: &str) -> Form {
        let mut fields = BTreeMap::new();
        let mut current: Option<String> = None;
        let mut in_options = false;
        for line in yaml.lines() {
            let trimmed = line.trim();
            if let Some(id) = trimmed.strip_prefix("id: ") {
                current = Some(id.trim().to_owned());
                fields.insert(id.trim().to_owned(), None);
                in_options = false;
            } else if trimmed == "options:" {
                in_options = true;
                if let Some(id) = &current {
                    fields.insert(id.clone(), Some(Vec::new()));
                }
            } else if in_options && let Some(option) = trimmed.strip_prefix("- ") {
                if let Some(Some(options)) = current.as_ref().and_then(|id| fields.get_mut(id)) {
                    options.push(option.trim().to_owned());
                }
            } else if !trimmed.is_empty() {
                in_options = false;
            }
        }
        fields
    }

    pub(crate) fn burn() -> Form {
        template(include_str!(
            "../../../.github/ISSUE_TEMPLATE/burn_report.yml"
        ))
    }

    pub(crate) fn hardware() -> Form {
        template(include_str!(
            "../../../.github/ISSUE_TEMPLATE/hardware_report.yml"
        ))
    }

    pub(crate) fn bug() -> Form {
        template(include_str!(
            "../../../.github/ISSUE_TEMPLATE/bug_report.yml"
        ))
    }

    /// Every field the report fills is on the form, and every dropdown value
    /// is one of that dropdown's options or empty, exactly: GitHub ignores a
    /// prefilled value it does not recognise.
    pub(crate) fn assert_fits(report: &IssueReport, form: &Form) {
        for field in &report.fields {
            let Some(options) = form.get(&field.id) else {
                panic!("{} is not a field of {}", field.id, report.template);
            };
            if let Some(options) = options
                && !field.value.is_empty()
            {
                assert!(
                    options.contains(&field.value),
                    "{:?} is not an option of {} in {}: {options:?}",
                    field.value,
                    field.id,
                    report.template
                );
            }
        }
    }
}

/// Why an issue address was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReportSettingsError {
    /// Not an `https://` address.
    #[error("the issue address must start with https://")]
    NotHttps,
    /// Something in it a plain issue-form address does not have.
    #[error("the issue address {0}")]
    Malformed(&'static str),
}

/// How issue reports are configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportSettings {
    issues_url: Option<String>,
    scrub_addresses: bool,
    build: BuildIdentity,
}

impl Default for ReportSettings {
    /// Reports off. A server only offers them once it is told where they go.
    fn default() -> Self {
        Self::disabled(BuildIdentity::current(None, None))
    }
}

impl ReportSettings {
    /// Reports that go to `issues_url`, or none at all if it is empty.
    ///
    /// The address is the issue form's own (`…/issues/new`), which a report
    /// adds `?template=…` and the prefilled fields to, so it must be a plain
    /// `https://` address with no query, fragment or credentials.
    ///
    /// # Errors
    ///
    /// When a non-empty address is not one of those.
    pub fn new(
        issues_url: &str,
        scrub_addresses: bool,
        build: BuildIdentity,
    ) -> Result<Self, ReportSettingsError> {
        let issues_url = issues_url.trim();
        let issues_url = if issues_url.is_empty() {
            None
        } else {
            Some(validate_issues_url(issues_url)?)
        };
        Ok(Self {
            issues_url,
            scrub_addresses,
            build,
        })
    }

    /// No reports, for a server that should not offer them.
    #[must_use]
    pub fn disabled(build: BuildIdentity) -> Self {
        Self {
            issues_url: None,
            scrub_addresses: true,
            build,
        }
    }

    /// Where issues go, when reports are on.
    #[must_use]
    pub fn issues_url(&self) -> Option<&str> {
        self.issues_url.as_deref()
    }

    /// Whether reports are offered at all.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.issues_url.is_some()
    }

    /// Whether addresses and private host names are scrubbed too.
    #[must_use]
    pub fn scrub_addresses(&self) -> bool {
        self.scrub_addresses
    }

    /// The build a report names.
    #[must_use]
    pub fn build(&self) -> &BuildIdentity {
        &self.build
    }

    /// A scrubbing context with this server's settings and no names yet.
    #[must_use]
    pub fn scrub_context(&self) -> ScrubContext {
        ScrubContext::new().scrub_addresses(self.scrub_addresses)
    }
}

fn validate_issues_url(url: &str) -> Result<String, ReportSettingsError> {
    let Some(rest) = url.strip_prefix("https://") else {
        return Err(ReportSettingsError::NotHttps);
    };
    if url.len() > MAX_ISSUES_URL_BYTES {
        return Err(ReportSettingsError::Malformed("is too long"));
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(ReportSettingsError::Malformed(
            "has a space or control character in it",
        ));
    }
    if url.contains(['?', '#']) {
        return Err(ReportSettingsError::Malformed(
            "must not have a query or fragment; reports add their own",
        ));
    }
    let host = rest.split('/').next().unwrap_or_default();
    if host.is_empty() {
        return Err(ReportSettingsError::Malformed("has no host"));
    }
    if host.contains('@') {
        return Err(ReportSettingsError::Malformed("must not carry credentials"));
    }
    Ok(url.trim_end_matches('/').to_owned())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn build() -> BuildIdentity {
        BuildIdentity::current(None, None)
    }

    #[test]
    fn the_default_address_is_accepted() {
        let settings = ReportSettings::new(DEFAULT_ISSUES_URL, true, build()).unwrap();
        assert_eq!(settings.issues_url(), Some(DEFAULT_ISSUES_URL));
        assert!(settings.enabled());
    }

    #[test]
    fn an_empty_address_turns_reports_off() {
        let settings = ReportSettings::new("  ", true, build()).unwrap();
        assert!(!settings.enabled());
        assert!(!ReportSettings::default().enabled());
    }

    #[test]
    fn a_trailing_slash_is_dropped() {
        let settings = ReportSettings::new(
            "https://git.example.org/me/tangible/issues/new/",
            true,
            build(),
        )
        .unwrap();
        assert_eq!(
            settings.issues_url(),
            Some("https://git.example.org/me/tangible/issues/new")
        );
    }

    #[test]
    fn addresses_a_report_cannot_be_added_to_are_refused() {
        for (url, error) in [
            (
                "http://github.com/x/y/issues/new",
                ReportSettingsError::NotHttps,
            ),
            ("github.com/x/y/issues/new", ReportSettingsError::NotHttps),
            (
                "https://github.com/x/y/issues/new?template=x",
                ReportSettingsError::Malformed(
                    "must not have a query or fragment; reports add their own",
                ),
            ),
            (
                "https://user:pass@github.com/x",
                ReportSettingsError::Malformed("must not carry credentials"),
            ),
            ("https:///x", ReportSettingsError::Malformed("has no host")),
            (
                "https://github.com/x y",
                ReportSettingsError::Malformed("has a space or control character in it"),
            ),
        ] {
            assert_eq!(ReportSettings::new(url, true, build()), Err(error), "{url}");
        }
        let long = format!("https://github.com/{}", "x".repeat(MAX_ISSUES_URL_BYTES));
        assert_eq!(
            ReportSettings::new(&long, true, build()),
            Err(ReportSettingsError::Malformed("is too long"))
        );
    }

    #[test]
    fn a_release_image_names_its_tag_and_commit() {
        let build = BuildIdentity::current(
            Some("v0.1.0"),
            Some("76fa5157aea012aae85461e694082d553071a606"),
        );
        assert_eq!(build.describe(), "v0.1.0 (76fa515)");
    }

    #[test]
    fn a_local_build_says_so() {
        let version = env!("CARGO_PKG_VERSION");
        assert_eq!(
            BuildIdentity::current(Some("dev"), Some("unknown")).describe(),
            format!("{version}, built locally")
        );
        assert_eq!(
            BuildIdentity::current(None, Some("abcdef0123")).describe(),
            format!("{version} (abcdef0), built locally")
        );
    }

    #[test]
    fn odd_build_fields_are_ignored_rather_than_repeated() {
        let build = BuildIdentity::current(Some("v1 <script>"), Some(&"a".repeat(65)));
        assert_eq!(build.release, None);
        assert_eq!(build.revision, None);
    }

    #[test]
    fn the_scrub_context_follows_the_address_setting() {
        let off = ReportSettings::new(DEFAULT_ISSUES_URL, false, build()).unwrap();
        let text = tangible_domain::report::scrub("at 192.168.1.13", &off.scrub_context()).text;
        assert_eq!(text, "at 192.168.1.13");
        let on = ReportSettings::new(DEFAULT_ISSUES_URL, true, build()).unwrap();
        let text = tangible_domain::report::scrub("at 192.168.1.13", &on.scrub_context()).text;
        assert_eq!(text, "at <address>");
    }
}

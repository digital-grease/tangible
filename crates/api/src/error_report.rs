// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! An error the web UI showed, written up as a bug report; and a report a
//! person starts themselves, from "Report an issue".
//!
//! The browser sends what it was shown: the problem document's code, title,
//! detail and request id, which API call failed, and which part of the UI it
//! was in. This fills in the bug form (`bug_report.yml`) from that and the
//! server's own build, with every free-text part scrubbed. A pure function,
//! tested against the form's YAML like the burn report.

use tangible_domain::report::ScrubContext;

use crate::burn_report::lowercase_first;
use crate::report::{BuildIdentity, IssueReport, Writer};

/// The bug form's file name.
pub const BUG_TEMPLATE: &str = "bug_report.yml";

/// The bug form's Area options.
pub const AREAS: [&str; 8] = [
    "import",
    "library",
    "catalog",
    "romm",
    "web",
    "api",
    "deployment",
    "other",
];

/// The bug form's Deployment option for an official release image.
pub(crate) const RELEASE_IMAGE: &str = "Release image with the Compose files";

/// What the browser was shown, and where.
#[derive(Debug, Clone)]
pub struct ErrorFacts {
    /// The server's build.
    pub build: BuildIdentity,
    /// The problem's stable code, such as `INTERNAL`.
    pub code: String,
    /// The problem's title.
    pub title: String,
    /// The problem's detail.
    pub detail: String,
    /// The HTTP status, if there was a response at all.
    pub status: Option<u16>,
    /// The request's identifier, from the problem document.
    pub request_id: Option<String>,
    /// The API call that failed, such as `GET /api/v1/burn-jobs/{id}`.
    pub endpoint: Option<String>,
    /// The part of the UI, one of [`AREAS`], if the browser named one.
    pub area: Option<String>,
}

/// Where a person was when they chose "Report an issue".
#[derive(Debug, Clone)]
pub struct IssueFacts {
    /// The server's build.
    pub build: BuildIdentity,
    /// The page, as a route with its identifiers taken out:
    /// `/library/{id}`.
    pub page: Option<String>,
    /// The part of Tangible, one of [`AREAS`], if the browser named one.
    pub area: Option<String>,
}

/// Whether `page` is a web UI route with its identifiers taken out, as
/// the browser sends it: `/` and at most 300 of lowercase letters, digits,
/// `/`, `_`, `-`, `.`, `{` and `}`. Nothing else is repeated into a report.
#[must_use]
pub fn is_route(page: &str) -> bool {
    page.starts_with('/')
        && page.len() <= 300
        && page
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "/_-.{}".contains(c))
}

/// Fill in the bug form for a report a person starts themselves: what
/// Tangible knows (its build, whether it is a release image, the page and
/// its area), and the rest left for them, as fauxx's form leaves it.
#[must_use]
pub fn issue_report(facts: &IssueFacts, context: ScrubContext) -> IssueReport {
    let mut writer = Writer::new(context);
    writer.plain("version", "Tangible version", &facts.build.describe());
    writer.plain(
        "deployment",
        "Deployment",
        if facts.build.release.is_some() {
            RELEASE_IMAGE
        } else {
            ""
        },
    );
    writer.plain("host", "Host", "");
    let area = facts
        .area
        .as_deref()
        .filter(|area| AREAS.contains(area))
        .unwrap_or("");
    writer.plain("area", "Area", area);
    writer.plain("what_happened", "What happened?", "");
    writer.plain("what_expected", "What did you expect to happen?", "");
    // Not scrubbed, which would take a page for a host path: the route
    // checks it is one, in the few characters a route has.
    let steps = facts
        .page
        .as_deref()
        .filter(|page| is_route(page))
        .map(|page| format!("1. On the page {page}: \n"))
        .unwrap_or_default();
    writer.plain("steps", "Steps to reproduce", &steps);
    writer.plain("logs", "Logs", "");
    writer.finish(BUG_TEMPLATE, None, None)
}

/// Fill in the bug form from `facts`, scrubbing with `context`.
#[must_use]
pub fn error_report(facts: &ErrorFacts, context: ScrubContext) -> IssueReport {
    let mut writer = Writer::new(context);
    writer.plain("version", "Tangible version", &facts.build.describe());
    // Only a release image can be said for certain; anything else the
    // person knows better than the server.
    writer.plain(
        "deployment",
        "Deployment",
        if facts.build.release.is_some() {
            RELEASE_IMAGE
        } else {
            ""
        },
    );
    writer.plain("host", "Host", "");
    let area = facts
        .area
        .as_deref()
        .filter(|area| AREAS.contains(area))
        .unwrap_or("");
    writer.plain("area", "Area", area);
    writer.scrubbed(
        "what_happened",
        "What happened?",
        &format!("Tangible showed: {} ({}).", facts.title, facts.code),
    );
    writer.plain("what_expected", "What did you expect to happen?", "");
    writer.plain("steps", "Steps to reproduce", "");

    let mut log = vec![format!("code: {}", facts.code)];
    if let Some(status) = facts.status {
        log.push(format!("status: {status}"));
    }
    log.push(format!("title: {}", facts.title));
    log.push(format!("detail: {}", facts.detail));
    if let Some(endpoint) = &facts.endpoint {
        log.push(format!("request: {endpoint}"));
    }
    if let Some(id) = &facts.request_id {
        log.push(format!("request id: {id}"));
    }
    writer.scrubbed("logs", "Logs", &(log.join("\n") + "\n"));

    writer.finish(
        BUG_TEMPLATE,
        Some(format!(
            "Bug: {} ({})",
            lowercase_first(&facts.title),
            facts.code
        )),
        Some("logs"),
    )
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::report::forms::{assert_fits, bug};

    fn facts() -> ErrorFacts {
        ErrorFacts {
            build: BuildIdentity::current(
                Some("v0.1.0"),
                Some("76fa5157aea012aae85461e694082d553071a606"),
            ),
            code: "INTERNAL".to_owned(),
            title: "Internal server error".to_owned(),
            detail: "could not read /var/lib/tangible/library/objects/ab/cd from 192.168.1.13"
                .to_owned(),
            status: Some(500),
            request_id: Some("01a126e9-2507-7133-8bfa-b724fc014c1f".to_owned()),
            endpoint: Some("GET /api/v1/artifacts/{artifact_id}".to_owned()),
            area: Some("library".to_owned()),
        }
    }

    fn field<'a>(report: &'a IssueReport, id: &str) -> &'a str {
        &report
            .fields
            .iter()
            .find(|field| field.id == id)
            .unwrap_or_else(|| panic!("no field {id}"))
            .value
    }

    #[test]
    fn an_error_fills_in_the_bug_form() {
        let report = error_report(&facts(), ScrubContext::new());
        assert_eq!(report.template, BUG_TEMPLATE);
        assert_eq!(report.long_field.as_deref(), Some("logs"));
        assert_eq!(field(&report, "version"), "v0.1.0 (76fa515)");
        assert_eq!(field(&report, "deployment"), RELEASE_IMAGE);
        assert_eq!(field(&report, "area"), "library");
        assert_eq!(
            field(&report, "what_happened"),
            "Tangible showed: Internal server error (INTERNAL)."
        );
        assert_eq!(
            report.title.as_deref(),
            Some("Bug: internal server error (INTERNAL)")
        );
        assert_fits(&report, &bug());
    }

    #[test]
    fn the_log_keeps_the_request_id_and_loses_paths_and_addresses() {
        let report = error_report(&facts(), ScrubContext::new());
        let logs = field(&report, "logs");
        assert!(
            logs.contains("request id: 01a126e9-2507-7133-8bfa-b724fc014c1f"),
            "{logs}"
        );
        assert!(
            logs.contains("request: GET /api/v1/artifacts/{artifact_id}"),
            "{logs}"
        );
        assert!(!logs.contains("/var/lib"), "{logs}");
        assert!(!logs.contains("192.168.1.13"), "{logs}");
        assert_eq!(report.removed.get("path"), Some(&1));
        assert_eq!(report.removed.get("address"), Some(&1));
    }

    #[test]
    fn a_local_build_and_an_unknown_area_are_left_for_the_person() {
        let mut facts = facts();
        facts.build = BuildIdentity::current(None, None);
        facts.area = Some("kitchen".to_owned());
        let report = error_report(&facts, ScrubContext::new());
        assert_eq!(field(&report, "deployment"), "");
        assert_eq!(field(&report, "area"), "");
        assert_fits(&report, &bug());
    }

    #[test]
    fn every_area_is_one_the_form_offers() {
        for area in AREAS {
            let mut facts = facts();
            facts.area = Some(area.to_owned());
            assert_fits(&error_report(&facts, ScrubContext::new()), &bug());
        }
    }

    #[test]
    fn a_report_started_by_a_person_says_what_tangible_knows_and_leaves_the_rest() {
        let facts = IssueFacts {
            build: BuildIdentity::current(
                Some("v0.1.0"),
                Some("76fa5157aea012aae85461e694082d553071a606"),
            ),
            page: Some("/library/{id}".to_owned()),
            area: Some("library".to_owned()),
        };
        let report = issue_report(&facts, ScrubContext::new());
        assert_eq!(report.template, BUG_TEMPLATE);
        assert_eq!(report.title, None, "the form's own title");
        assert_eq!(report.long_field, None);
        assert_eq!(field(&report, "version"), "v0.1.0 (76fa515)");
        assert_eq!(field(&report, "deployment"), RELEASE_IMAGE);
        assert_eq!(field(&report, "area"), "library");
        assert_eq!(field(&report, "steps"), "1. On the page /library/{id}: \n");
        for left in ["host", "what_happened", "what_expected", "logs"] {
            assert_eq!(field(&report, left), "", "{left}");
        }
        assert_fits(&report, &bug());

        // From a local build, on no page in particular, in no known area.
        let local = IssueFacts {
            build: BuildIdentity::current(None, None),
            page: None,
            area: Some("kitchen".to_owned()),
        };
        let report = issue_report(&local, ScrubContext::new());
        assert!(is_route("/requests/{id}") && is_route("/"));
        for not in ["library", "/Library", "/a b", "/x?y=1", "/x\u{7}", ""] {
            assert!(!is_route(not), "{not:?}");
        }
        assert!(!is_route(&format!("/{}", "a".repeat(300))));
        assert_eq!(field(&report, "deployment"), "");
        assert_eq!(field(&report, "area"), "");
        assert_eq!(field(&report, "steps"), "");
        assert_fits(&report, &bug());
    }
}

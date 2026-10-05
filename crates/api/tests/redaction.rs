// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Nothing that carries a secret prints it.
//!
//! Every type here holds a password, a token, a credential or a connection
//! URL with a password in it, and every one is formatted with `{:?}` the way
//! a tracing field, an `expect` message or a failed assertion would format
//! it. The value must not appear. Types that carry a secret but have no
//! `Debug` at all (the enrollment and session responses) cannot be formatted
//! by accident, which the compiler checks.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use tangible_api::auth::CurrentUser;
use tangible_api::routes::accounts::{CreateUserRequest, Credentials};
use tangible_api::routes::workers::EnrollmentRequest;
use tangible_db::DbConfig;
use tangible_db::accounts::{SessionRecord, UserRecord};
use tangible_domain::Role;
use time::OffsetDateTime;

const PLACEHOLDER: &str = "placeholder-value-for-redaction";

fn assert_hidden(shown: &str) {
    assert!(!shown.contains(PLACEHOLDER), "printed the secret: {shown}");
    assert!(
        shown.contains("[redacted]"),
        "says nothing was there: {shown}"
    );
}

#[test]
fn passwords_in_request_bodies_never_print() {
    let credentials: Credentials = serde_json::from_value(serde_json::json!({
        "username": "owner",
        "password": PLACEHOLDER,
    }))
    .unwrap();
    assert_hidden(&format!("{credentials:?}"));
    assert_eq!(credentials.password.expose(), PLACEHOLDER);

    let create: CreateUserRequest = serde_json::from_value(serde_json::json!({
        "username": "someone",
        "password": PLACEHOLDER,
        "role": "viewer",
    }))
    .unwrap();
    assert_hidden(&format!("{create:?}"));
}

#[test]
fn an_enrollment_token_in_a_request_never_prints() {
    let request: EnrollmentRequest = serde_json::from_value(serde_json::json!({
        "enrollment_token": PLACEHOLDER,
        "name": "burner",
        "protocol_versions": ["1"],
        "software_version": "0.1.0",
    }))
    .unwrap();
    assert_hidden(&format!("{request:?}"));
}

#[test]
fn a_session_never_prints_its_anti_forgery_token() {
    let record = SessionRecord {
        id: uuid::Uuid::nil(),
        csrf_token: PLACEHOLDER.to_owned(),
        expires_at: OffsetDateTime::UNIX_EPOCH,
        user: UserRecord {
            id: uuid::Uuid::nil(),
            username: "owner".to_owned(),
            role: Role::Administrator,
            disabled: false,
            last_signed_in_at: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        },
    };
    assert_hidden(&format!("{record:?}"));
    let user = CurrentUser::from(record);
    assert_hidden(&format!("{user:?}"));
}

#[test]
fn the_database_url_never_prints() {
    let config = DbConfig::new(format!("postgres://tangible:{PLACEHOLDER}@db/tangible"));
    assert_hidden(&format!("{config:?}"));
}

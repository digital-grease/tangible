// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Cursor pagination.
//!
//! Keyset, not offset. Artifact identifiers are UUIDv7 and therefore
//! time-ordered, so "everything after this id" is a stable window even while
//! the collection is being written to. Offset pagination over a growing
//! collection silently skips or repeats rows, which for a library someone is
//! importing into is a guarantee worth having.
//!
//! Cursors are opaque by construction: base64url of the last identifier seen.
//! Opaque so callers do not build them by hand and become dependent on the
//! encoding, which would freeze it.

// `Problem` is an HTTP response document, not a hot-path error. It is
// constructed once at the request boundary and immediately serialized, so its
// size costs nothing; boxing it would add an allocation and obscure every
// handler signature.
#![allow(clippy::result_large_err)]

use serde::{Deserialize, Serialize};
use utoipa::IntoParams;

use crate::problem::{ErrorCode, Problem};

/// Default page size when the caller does not ask.
pub const DEFAULT_LIMIT: usize = 50;

/// Largest page a caller may request.
///
/// Bounded so one request cannot ask the server to materialize the entire
/// library.
pub const MAX_LIMIT: usize = 500;

/// Query parameters accepted by every list endpoint.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct PageQuery {
    /// Maximum items to return. Clamped to [`MAX_LIMIT`].
    pub limit: Option<usize>,
    /// Opaque cursor from a previous response's `next_cursor`.
    pub cursor: Option<String>,
}

impl PageQuery {
    /// The effective page size.
    #[must_use]
    pub fn limit(&self) -> usize {
        // Clamped rather than rejected: an over-large limit is a client being
        // optimistic, not an error worth failing the request over.
        self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
    }

    /// Decode the cursor, if one was supplied.
    ///
    /// # Errors
    ///
    /// A problem with code `INVALID_CURSOR` if the cursor is not one this
    /// server could have issued.
    pub fn after(&self) -> Result<Option<String>, Problem> {
        match &self.cursor {
            None => Ok(None),
            Some(raw) => decode_cursor(raw).map(Some),
        }
    }
}

/// One page of results.
///
/// Generic for reuse, but deliberately not a schema type. Each resource
/// declares its own concrete page struct so the published OpenAPI names a real
/// type rather than an anonymous generic instantiation.
#[derive(Debug, Clone, Serialize)]
pub struct Page<T> {
    /// The items in this page.
    pub items: Vec<T>,
    /// Cursor for the next page, or null when this is the last one.
    ///
    /// Null rather than an empty string so a client can test it directly
    /// without a special case.
    pub next_cursor: Option<String>,
}

impl<T> Page<T> {
    /// Build a page, deriving the cursor from the last item.
    ///
    /// `key_of` extracts the ordering key. The cursor is only present when the
    /// page was filled, since a short page is by definition the last one.
    pub fn new(items: Vec<T>, limit: usize, key_of: impl Fn(&T) -> String) -> Self {
        let next_cursor = if items.len() < limit {
            None
        } else {
            items.last().map(|last| encode_cursor(&key_of(last)))
        };
        Self { items, next_cursor }
    }
}

/// Encode an ordering key as an opaque cursor.
#[must_use]
pub fn encode_cursor(key: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key)
}

/// Decode an opaque cursor.
///
/// # Errors
///
/// A problem with code `INVALID_CURSOR` if it is not decodable.
pub fn decode_cursor(cursor: &str) -> Result<String, Problem> {
    use base64::Engine as _;
    let invalid = || {
        Problem::new(
            ErrorCode::InvalidCursor,
            "the cursor was not issued by this server",
        )
    };
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| invalid())?;
    String::from_utf8(bytes).map_err(|_| invalid())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_round_trips() {
        let key = "0198a5bf-1db6-7f2e-9c20-6cc74f960901";
        assert_eq!(decode_cursor(&encode_cursor(key)).expect("decode"), key);
    }

    #[test]
    fn a_cursor_is_not_the_raw_key() {
        // Opaque so callers do not construct them by hand and freeze the
        // encoding.
        let key = "0198a5bf-1db6-7f2e-9c20-6cc74f960901";
        assert_ne!(encode_cursor(key), key);
    }

    #[test]
    fn a_forged_cursor_is_rejected() {
        for bad in ["!!!!", "not base64!", " "] {
            assert!(decode_cursor(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn a_cursor_carrying_invalid_utf8_is_rejected() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0xff_u8, 0xfe]);
        assert!(decode_cursor(&encoded).is_err());
    }

    #[test]
    fn the_limit_is_clamped_rather_than_rejected() {
        assert_eq!(PageQuery::default().limit(), DEFAULT_LIMIT);
        assert_eq!(
            PageQuery {
                limit: Some(usize::MAX),
                cursor: None
            }
            .limit(),
            MAX_LIMIT
        );
        // Zero would make progress impossible.
        assert_eq!(
            PageQuery {
                limit: Some(0),
                cursor: None
            }
            .limit(),
            1
        );
    }

    #[test]
    fn a_short_page_has_no_next_cursor() {
        // A page shorter than the limit is the last one by definition.
        let page = Page::new(vec!["a".to_owned(), "b".to_owned()], 10, Clone::clone);
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn a_full_page_carries_a_cursor_from_its_last_item() {
        let page = Page::new(vec!["a".to_owned(), "b".to_owned()], 2, Clone::clone);
        let cursor = page
            .next_cursor
            .as_deref()
            .expect("a full page has a cursor");
        assert_eq!(decode_cursor(cursor).expect("decode"), "b");
    }

    #[test]
    fn an_empty_page_has_no_cursor() {
        let page: Page<String> = Page::new(vec![], 10, Clone::clone);
        assert!(page.next_cursor.is_none());
        assert!(page.items.is_empty());
    }
}

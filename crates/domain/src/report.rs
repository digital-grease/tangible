// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Text made safe to put in a public issue report.
//!
//! An operator can turn a burn attempt, an error or a diagnostics summary
//! into a GitHub issue. The text that goes into one comes from tool output,
//! error details and logs, and any of those can carry a credential, a path
//! on the host, a file name that says what is in someone's library, a drive
//! serial number or a LAN address. [`scrub`] replaces each of those with a
//! neutral placeholder before the text is shown for review, so the web UI
//! and the command line both offer the same scrubbed text.
//!
//! No scrubber is complete. This one is the first line; the second is that
//! every report is shown in full, with what was removed, before a person
//! decides to send it.
//!
//! Pure string work, bounded in size, and fuzzed.

use std::collections::BTreeMap;

use serde::Serialize;

/// The most text [`scrub`] reads; anything after it is dropped, and the
/// result says so.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

/// The most text [`scrub`] returns. Labels can be longer than the names
/// they replace, so the output has its own bound.
pub const MAX_OUTPUT_BYTES: usize = 2 * MAX_INPUT_BYTES;

/// Names shorter than this are not replaced: a two-letter name would match
/// inside ordinary words and make the text unreadable.
pub const MIN_NAME_CHARS: usize = 3;

/// The longest label a name is replaced with.
pub const MAX_LABEL_CHARS: usize = 32;

/// The prefixes Tangible's own secrets start with: worker credentials,
/// enrollment tokens and session tokens.
pub const CREDENTIAL_PREFIXES: [&str; 3] = ["tgw_live_", "tgw_enroll_", "tgs_"];

/// Headers whose value is always a secret.
const SECRET_HEADERS: [&str; 5] = [
    "proxy-authorization:",
    "authorization:",
    "set-cookie:",
    "cookie:",
    "x-csrf-token:",
];

/// Schemes whose next word is a credential.
const AUTH_SCHEMES: [&str; 2] = ["bearer ", "basic "];

/// The session cookie, wherever it appears with a value.
const SESSION_COOKIE: &str = "tangible_session=";

/// Words a serial number follows.
const SERIAL_KEYS: [&str; 2] = ["drive id", "serial"];

/// Host suffixes that only make sense on a private network.
const PRIVATE_HOST_SUFFIXES: [&str; 5] =
    [".lan", ".local", ".localdomain", ".home.arpa", ".internal"];

/// Absolute paths kept as they are: device nodes and Tangible's own routes
/// say what was involved without saying anything about the host.
const KEPT_PATH_PREFIXES: [&str; 2] = ["/dev/", "/api/"];

/// Every placeholder this module writes, so callers and tests can tell its
/// own words from leftover input.
pub const PLACEHOLDERS: [&str; 7] = [
    "<credential>",
    "<redacted>",
    "<credentials>",
    "<path>",
    "<serial>",
    "<address>",
    "<host>",
];

/// A kind of thing [`scrub`] removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Removed {
    /// A Tangible credential, enrollment token or session token.
    Credential,
    /// The value of an authorization or cookie header, or a bearer token.
    HeaderValue,
    /// A user name and password inside a URL.
    UrlCredentials,
    /// An absolute path on a host.
    Path,
    /// A name the caller said to replace, such as a file in the library.
    Name,
    /// A drive or disc serial number.
    Serial,
    /// An IP address.
    Address,
    /// A host name on a private network.
    Host,
}

/// What [`scrub`] needs to know beyond the text itself.
#[derive(Debug, Clone)]
pub struct ScrubContext {
    /// Name and the label replacing it, longest name first.
    names: Vec<(String, String)>,
    scrub_addresses: bool,
}

impl Default for ScrubContext {
    fn default() -> Self {
        Self::new()
    }
}

impl ScrubContext {
    /// A context with no names, which scrubs addresses.
    #[must_use]
    pub fn new() -> Self {
        Self {
            names: Vec::new(),
            scrub_addresses: true,
        }
    }

    /// Whether to replace IP addresses and private host names.
    #[must_use]
    pub fn scrub_addresses(mut self, on: bool) -> Self {
        self.scrub_addresses = on;
        self
    }

    /// Replace every occurrence of `name` with `label`.
    ///
    /// The label is reduced to letters, digits and hyphens, at most
    /// [`MAX_LABEL_CHARS`] long, and written in angle brackets:
    /// `component 2` becomes `<component-2>`. A name shorter than
    /// [`MIN_NAME_CHARS`], once trimmed, is ignored, as is a second label for
    /// a name already given one.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>, label: &str) -> Self {
        let name = name.into();
        if name.trim().chars().count() < MIN_NAME_CHARS
            || self.names.iter().any(|(known, _)| *known == name)
        {
            return self;
        }
        let label = label_for(label);
        let at = self
            .names
            .iter()
            .position(|(known, _)| known.len() < name.len())
            .unwrap_or(self.names.len());
        self.names.insert(at, (name, label));
        self
    }
}

/// `component 2` as `<component-2>`.
fn label_for(label: &str) -> String {
    let mut out = String::from("<");
    for c in label.chars() {
        if out.chars().count() > MAX_LABEL_CHARS {
            break;
        }
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if (c == ' ' || c == '-' || c == '_') && !out.ends_with('-') && out.len() > 1 {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.len() == 1 {
        out.push_str("name");
    }
    out.push('>');
    out
}

/// Scrubbed text, and what was taken out of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Scrubbed {
    /// The text with every match replaced.
    pub text: String,
    /// How many of each kind were replaced.
    pub removed: BTreeMap<Removed, usize>,
    /// Whether the input or the output was cut short.
    pub truncated: bool,
}

/// Replace everything in `text` that should not appear in a public report.
///
/// In order: the context's names, Tangible's credentials, secret header
/// values and bearer tokens, credentials in URLs, serial numbers, addresses
/// and private host names if the context asks, absolute paths (keeping a
/// file extension), and Tangible's credentials once more, so nothing a step
/// leaves behind can still carry one.
#[must_use]
pub fn scrub(text: &str, context: &ScrubContext) -> Scrubbed {
    let mut removed = BTreeMap::new();
    let (input, mut truncated) = cut(text, MAX_INPUT_BYTES);

    let mut out = names(input, &context.names, &mut removed);
    // Credentials early, so what follows one is seen by the later steps, and
    // again last, as the guarantee.
    out = credentials(&out, &mut removed);
    out = header_values(&out, &mut removed);
    out = url_credentials(&out, &mut removed);
    out = serials(&out, &mut removed);
    // Hosts before paths: in `nas.lan/share/disc.iso` the path only starts
    // once the host in front of it has become a placeholder.
    if context.scrub_addresses {
        out = addresses(&out, &mut removed);
    }
    out = paths(&out, &mut removed);
    out = credentials(&out, &mut removed);

    let (bounded, cut_output) = cut(&out, MAX_OUTPUT_BYTES);
    truncated |= cut_output;
    Scrubbed {
        text: bounded.to_owned(),
        removed,
        truncated,
    }
}

/// At most `max` bytes of `text`, cut on a character boundary.
fn cut(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

fn count(removed: &mut BTreeMap<Removed, usize>, kind: Removed) {
    *removed.entry(kind).or_insert(0) += 1;
}

/// Each name, longest first, in one left-to-right pass, so a label already
/// written is never matched again.
fn names(text: &str, names: &[(String, String)], removed: &mut BTreeMap<Removed, usize>) -> String {
    if names.is_empty() {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    'scan: while let Some(c) = rest.chars().next() {
        for (name, label) in names {
            if rest.starts_with(name.as_str()) {
                out.push_str(label);
                rest = &rest[name.len()..];
                count(removed, Removed::Name);
                continue 'scan;
            }
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// Byte index of `needle` in `haystack`, ignoring ASCII case.
fn find_ascii_ci(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let hay = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (from..=hay.len() - needle.len())
        .find(|&i| hay[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

/// Whether the text after a key is a value worth replacing: not empty, and
/// not already nothing but a placeholder this module or a name label wrote.
fn has_value(after_key: &str) -> bool {
    let value = after_key.trim();
    let placeholder = value.len() > 2
        && value.starts_with('<')
        && value.ends_with('>')
        && value[1..value.len() - 1]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-');
    !value.is_empty() && !placeholder
}

/// A line and its ending, split so the ending survives a replacement.
fn split_ending(line: &str) -> (&str, &str) {
    let body = line.trim_end_matches(['\n', '\r']);
    (body, &line[body.len()..])
}

/// Characters a token, credential or cookie value is made of.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '~' | '+' | '/' | '=' | '%')
}

/// The byte length of the token at the start of `text`.
fn token_len(text: &str) -> usize {
    text.find(|c: char| !is_token_char(c)).unwrap_or(text.len())
}

/// The value of a secret header, to the end of its line; a bearer or basic
/// token; and the session cookie's value.
fn header_values(text: &str, removed: &mut BTreeMap<Removed, usize>) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (body, ending) = split_ending(line);
        let header = SECRET_HEADERS
            .iter()
            .filter_map(|name| find_ascii_ci(body, name, 0).map(|at| at + name.len()))
            .min();
        if let Some(value_at) = header
            && has_value(&body[value_at..])
        {
            // What comes before the header on its line can hold a token too.
            out.push_str(&inline_secrets(&body[..value_at], removed));
            out.push_str(" <redacted>");
            out.push_str(ending);
            count(removed, Removed::HeaderValue);
            continue;
        }
        out.push_str(&inline_secrets(body, removed));
        out.push_str(ending);
    }
    out
}

/// Bearer and basic tokens, and the session cookie, within one line.
fn inline_secrets(line: &str, removed: &mut BTreeMap<Removed, usize>) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    loop {
        let next = AUTH_SCHEMES
            .iter()
            .chain(std::iter::once(&SESSION_COOKIE))
            .filter_map(|key| find_ascii_ci(rest, key, 0).map(|at| (at, key.len())))
            .min();
        let Some((at, key_len)) = next else {
            out.push_str(rest);
            return out;
        };
        let value_at = at + key_len;
        let len = token_len(&rest[value_at..]);
        let token = &rest[value_at..value_at + len];
        out.push_str(&rest[..value_at]);
        if is_secret_token(&rest[at..value_at], token) {
            out.push_str("<redacted>");
            count(removed, Removed::HeaderValue);
        } else {
            out.push_str(token);
        }
        rest = &rest[value_at + len..];
    }
}

/// Whether `token`, following `key`, is a secret. Any session cookie value
/// is. After `bearer` or `basic` only something token-shaped is, at least
/// eight characters with a digit in them, so "basic configuration" and "the
/// bearer of bad news" stay as they are.
fn is_secret_token(key: &str, token: &str) -> bool {
    if key.eq_ignore_ascii_case(SESSION_COOKIE) {
        return !token.is_empty();
    }
    token.len() >= 8 && token.chars().any(|c| c.is_ascii_digit())
}

/// `scheme://user:password@host` as `scheme://<credentials>@host`.
fn url_credentials(text: &str, removed: &mut BTreeMap<Removed, usize>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let authority_at = at + 3;
        out.push_str(&rest[..authority_at]);
        rest = &rest[authority_at..];
        let end = rest
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '/' | '?' | '#' | '"' | '\'' | '<' | '>' | '`')
            })
            .unwrap_or(rest.len());
        if let Some(userinfo_end) = rest[..end].rfind('@')
            && userinfo_end > 0
        {
            out.push_str("<credentials>");
            rest = &rest[userinfo_end..];
            count(removed, Removed::UrlCredentials);
        }
    }
    out.push_str(rest);
    out
}

/// Whether a path, a host or an address may start after `previous`.
fn is_boundary(previous: Option<char>) -> bool {
    previous.is_none_or(|c| {
        c.is_whitespace()
            || matches!(
                c,
                '"' | '\'' | '(' | '[' | '{' | '=' | ',' | ';' | '`' | '>'
            )
    })
}

/// Characters that end a path.
fn ends_path(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '\'' | '<' | '>' | '`' | ')' | ']' | '}' | ',' | ';'
        )
}

/// A file extension worth keeping: one to five letters or digits.
fn extension(path: &str) -> Option<&str> {
    let file = path.rsplit(['/', '\\']).next()?;
    let (stem, ext) = file.rsplit_once('.')?;
    (!stem.is_empty()
        && (1..=5).contains(&ext.len())
        && ext.chars().all(|c| c.is_ascii_alphanumeric()))
    .then_some(ext)
}

/// Where a path begins in `rest`, if one does: `/`, `~/` or `C:\`.
fn path_start(rest: &str) -> bool {
    let b = rest.as_bytes();
    match b {
        [b'/', next, ..] => *next != b'/' && !next.is_ascii_whitespace(),
        [b'~', b'/', ..] => true,
        [drive, b':', b'\\', ..] => drive.is_ascii_alphabetic(),
        _ => false,
    }
}

/// How many words after a path's first word can still belong to it.
const MAX_PATH_WORDS: usize = 8;

/// Punctuation that ends a sentence or clause, or closes a bracket, rather
/// than belonging to the path before it.
const TRAILING_PUNCTUATION: [char; 9] = [':', '.', ',', ';', '!', '?', ')', ']', '}'];

/// Characters that end a word after a path's first: whitespace and quotes.
/// Brackets do not, since `(USA)` is part of a file name.
fn ends_word(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`')
}

/// Whether a word looks like part of a path: it has a separator in it, or
/// ends in a file extension once trailing punctuation is set aside.
fn continues_path(word: &str) -> bool {
    let word = word.trim_end_matches(TRAILING_PUNCTUATION);
    word.contains(['/', '\\']) || extension(word).is_some()
}

/// The byte length of the path at the start of `rest`.
///
/// A path's first word runs to whitespace or a quote. File names in disc
/// collections have spaces in them (`Some Game (USA) (Track 1).bin`), so the
/// path carries on through up to [`MAX_PATH_WORDS`] more words on the same
/// line while a later one still looks like part of a path. When that is
/// unclear it takes too much rather than too little: the preview shows the
/// result, and a lost word is better than a leaked name. Trailing sentence
/// punctuation is left outside.
fn path_len(rest: &str) -> usize {
    let first = rest.find(ends_path).unwrap_or(rest.len());
    let mut end = first;
    let mut at = first;
    for _ in 0..MAX_PATH_WORDS {
        let after = &rest[at..];
        // Only plain spaces join words into one path; a tab, a newline or a
        // quote ends it.
        let spaces = after.len() - after.trim_start_matches(' ').len();
        if spaces == 0 || spaces == after.len() {
            break;
        }
        let word_at = at + spaces;
        let word_len = rest[word_at..]
            .find(ends_word)
            .unwrap_or(rest.len() - word_at);
        if word_len == 0 {
            break;
        }
        let word = &rest[word_at..word_at + word_len];
        if continues_path(word) {
            end = word_at + word_len;
        }
        at = word_at + word_len;
        if word.ends_with([':', ',', ';']) {
            break;
        }
    }
    let path = rest[..end].trim_end_matches(TRAILING_PUNCTUATION);
    path.len()
}

/// Absolute paths as `<path>`, keeping a file extension (`<path>.bin`).
/// Device nodes and Tangible's own API routes stay.
fn paths(text: &str, removed: &mut BTreeMap<Removed, usize>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut previous = None;
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if is_boundary(previous) && path_start(rest) {
            let len = path_len(rest);
            let path = &rest[..len];
            if KEPT_PATH_PREFIXES.iter().any(|kept| path.starts_with(kept)) {
                out.push_str(path);
            } else {
                out.push_str("<path>");
                if let Some(ext) = extension(path) {
                    out.push('.');
                    out.push_str(ext);
                }
                count(removed, Removed::Path);
            }
            previous = path.chars().last();
            rest = &rest[len..];
            continue;
        }
        out.push(c);
        previous = Some(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// `Drive id : 'ABC123'` or `Serial number: ABC123` as `<serial>`, to the end
/// of the line.
fn serials(text: &str, removed: &mut BTreeMap<Removed, usize>) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (body, ending) = split_ending(line);
        match serial_value_at(body) {
            Some(at) => {
                out.push_str(&body[..at]);
                out.push_str(" <serial>");
                count(removed, Removed::Serial);
            }
            _ => out.push_str(body),
        }
        out.push_str(ending);
    }
    out
}

/// Where the value after a serial-number key begins: a key, then up to
/// twenty letters, spaces, hyphens or underscores, then `:` or `=`, then
/// something. The earliest key with a value wins, so a line holding two
/// keys loses everything after the first that has one.
fn serial_value_at(line: &str) -> Option<usize> {
    let mut earliest: Option<usize> = None;
    for key in SERIAL_KEYS {
        let mut from = 0;
        while let Some(at) = find_ascii_ci(line, key, from) {
            let after_key = at + key.len();
            from = after_key;
            if earliest.is_some_and(|known| known <= at)
                || line[..at]
                    .chars()
                    .last()
                    .is_some_and(|c| c.is_ascii_alphanumeric())
            {
                continue;
            }
            let gap = line[after_key..]
                .char_indices()
                .take_while(|(i, c)| {
                    *i <= 20 && (c.is_ascii_alphabetic() || matches!(c, ' ' | '-' | '_' | '\t'))
                })
                .last()
                .map_or(0, |(i, c)| i + c.len_utf8());
            let separator_at = after_key + gap;
            if matches!(line[separator_at..].chars().next(), Some(':' | '='))
                && has_value(&line[separator_at + 1..])
            {
                earliest = Some(separator_at + 1);
                break;
            }
        }
    }
    earliest
}

/// Characters an address or host name token is made of.
fn is_address_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '-' | '%' | '_')
}

/// IPv4 and IPv6 addresses as `<address>`, private host names as `<host>`.
fn addresses(text: &str, removed: &mut BTreeMap<Removed, usize>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut previous = None;
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if is_address_char(c) && previous.is_none_or(|p: char| !is_address_char(p)) {
            let len = rest
                .find(|c: char| !is_address_char(c))
                .unwrap_or(rest.len());
            let token = &rest[..len];
            let (replaced, last) = address_token(token, removed);
            out.push_str(&replaced);
            previous = last;
            rest = &rest[len..];
            continue;
        }
        out.push(c);
        previous = Some(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// One run of address characters, with its trailing punctuation kept apart:
/// a sentence ending `192.168.1.13.` keeps its full stop.
fn address_token(token: &str, removed: &mut BTreeMap<Removed, usize>) -> (String, Option<char>) {
    let core = token.trim_end_matches(['.', ':', '-', '_']);
    let tail = &token[core.len()..];
    let last = token.chars().last();
    if let Some((address, port)) = split_port(core)
        && is_ipv4(address)
    {
        count(removed, Removed::Address);
        return (format!("<address>{port}{tail}"), last);
    }
    if is_ipv6(core) {
        count(removed, Removed::Address);
        return (format!("<address>{tail}"), last);
    }
    let host = split_port(core).map_or(core, |(host, _)| host);
    let lower = host.to_ascii_lowercase();
    if PRIVATE_HOST_SUFFIXES
        .iter()
        .any(|suffix| lower.len() > suffix.len() && lower.ends_with(suffix))
    {
        let port = &core[host.len()..];
        count(removed, Removed::Host);
        return (format!("<host>{port}{tail}"), last);
    }
    (token.to_owned(), last)
}

/// `host:8080` as `("host", ":8080")`; a token without a numeric port as
/// itself and an empty port.
fn split_port(token: &str) -> Option<(&str, &str)> {
    match token.rsplit_once(':') {
        Some((host, port))
            if !host.contains(':')
                && !port.is_empty()
                && port.len() <= 5
                && port.chars().all(|c| c.is_ascii_digit()) =>
        {
            Some((host, &token[host.len()..]))
        }
        _ if !token.contains(':') => Some((token, "")),
        _ => None,
    }
}

fn is_ipv4(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|part| {
            (1..=3).contains(&part.len())
                && part.chars().all(|c| c.is_ascii_digit())
                && part.parse::<u16>().is_ok_and(|n| n <= 255)
        })
}

/// An IPv6 address: hexadecimal groups and colons, with at least one digit,
/// and either `::` or all eight groups. Times such as `04:25:21`, Rust paths
/// such as `Self::new` (whose letters run into the word before) and MAC
/// addresses do not qualify.
fn is_ipv6(text: &str) -> bool {
    let address = text.split_once('%').map_or(text, |(address, _)| address);
    let colons = address.matches(':').count();
    address.len() >= 2
        && address.chars().all(|c| c.is_ascii_hexdigit() || c == ':')
        && address.chars().any(|c| c.is_ascii_digit())
        && !address.contains(":::")
        && ((address.contains("::") && colons <= 7) || colons == 7)
        && address.split(':').all(|group| group.len() <= 4)
}

/// Every Tangible credential prefix and the token after it as
/// `<credential>`. Last, so nothing the earlier steps left can carry one.
fn credentials(text: &str, removed: &mut BTreeMap<Removed, usize>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let next = CREDENTIAL_PREFIXES
            .iter()
            .filter_map(|prefix| rest.find(prefix).map(|at| (at, prefix.len())))
            .min();
        let Some((at, prefix_len)) = next else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..at]);
        out.push_str("<credential>");
        count(removed, Removed::Credential);
        let token_at = at + prefix_len;
        let len = rest[token_at..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(rest.len() - token_at);
        rest = &rest[token_at + len..];
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn plain(text: &str) -> Scrubbed {
        scrub(text, &ScrubContext::new())
    }

    fn removed(scrubbed: &Scrubbed, kind: Removed) -> usize {
        scrubbed.removed.get(&kind).copied().unwrap_or(0)
    }

    #[test]
    fn credentials_are_removed_wherever_they_appear() {
        let token = format!("tgw_live_{}", "ab12".repeat(16));
        let out = plain(&format!(
            "worker started with {token}; enroll tgw_enroll_00ff and tgs_xyz-9 end"
        ));
        assert_eq!(
            out.text,
            "worker started with <credential>; enroll <credential> and <credential> end"
        );
        assert_eq!(removed(&out, Removed::Credential), 3);
    }

    #[test]
    fn a_bare_prefix_is_removed_too() {
        assert_eq!(plain("prefix tgs_ alone").text, "prefix <credential> alone");
    }

    #[test]
    fn secret_header_values_are_removed_to_the_end_of_the_line() {
        let out = plain(
            "Authorization: Bearer abc.def\ncookie: tangible_session=xyz; other=1\r\nX-CSRF-Token: 1234\nAccept: */*\n",
        );
        assert_eq!(
            out.text,
            "Authorization: <redacted>\ncookie: <redacted>\r\nX-CSRF-Token: <redacted>\nAccept: */*\n"
        );
        assert_eq!(removed(&out, Removed::HeaderValue), 3);
    }

    #[test]
    fn a_token_before_a_header_on_the_same_line_is_removed() {
        // Found by the fuzzer: only the header's value was replaced.
        assert_eq!(
            plain("retry with Bearer abc12345 then Authorization: x").text,
            "retry with Bearer <redacted> then Authorization: <redacted>"
        );
    }

    #[test]
    fn inline_bearer_tokens_and_session_cookies_are_removed() {
        let out = plain("sent bearer eyJhbGciOiJIUzI1NiJ9.x-y and tangible_session=abc123 twice");
        assert_eq!(
            out.text,
            "sent bearer <redacted> and tangible_session=<redacted> twice"
        );
    }

    #[test]
    fn prose_after_bearer_or_basic_stays() {
        for kept in [
            "basic configuration passed",
            "the bearer of bad news",
            "basic 12",
        ] {
            assert_eq!(plain(kept).text, kept, "{kept}");
        }
    }

    #[test]
    fn url_credentials_are_removed_and_the_rest_of_the_url_kept() {
        let out = plain("DATABASE_URL=postgres://tangible:hunter2@db:5432/tangible failed");
        assert_eq!(
            out.text,
            "DATABASE_URL=postgres://<credentials>@db:5432/tangible failed"
        );
        assert_eq!(removed(&out, Removed::UrlCredentials), 1);
        assert_eq!(
            plain("see https://github.com/digital-grease/tangible/issues").text,
            "see https://github.com/digital-grease/tangible/issues"
        );
    }

    #[test]
    fn absolute_paths_keep_only_their_extension() {
        let out =
            plain("could not read staged file /var/lib/tangible/staging/0198/disc.bin: denied");
        assert_eq!(out.text, "could not read staged file <path>.bin: denied");
        assert_eq!(plain("open '/home/someone/x'").text, "open '<path>'");
        assert_eq!(
            plain(r"from C:\Users\someone\disc.iso").text,
            "from <path>.iso"
        );
        assert_eq!(
            plain("from ~/discs/one.cue now").text,
            "from <path>.cue now"
        );
        assert_eq!(plain("at /tmp").text, "at <path>");
    }

    #[test]
    fn a_path_with_spaces_in_it_goes_as_a_whole() {
        assert_eq!(
            plain("could not read /staging/0198/Some Game (USA) (Track 1).bin: denied").text,
            "could not read <path>.bin: denied"
        );
        assert_eq!(plain("read /tmp/x failed").text, "read <path> failed");
        // Ambiguous: takes too much rather than too little.
        assert_eq!(
            plain("copied /a/b to /c/d.iso today").text,
            "copied <path>.iso today"
        );
        // A newline ends a path.
        assert_eq!(plain("at /a/b\nnext.iso").text, "at <path>\nnext.iso");
    }

    #[test]
    fn device_nodes_routes_and_ordinary_slashes_stay() {
        for kept in [
            "drive /dev/sr0 is busy",
            "GET /api/v1/burn-jobs failed",
            "TRACK 01 MODE1/2352",
            "half done, 1/2",
            "see https://example.org/a/b",
            "a / b",
        ] {
            assert_eq!(plain(kept).text, kept, "{kept}");
        }
    }

    #[test]
    fn names_from_the_context_become_labels_longest_first() {
        let context = ScrubContext::new()
            .name("Some Game (USA)", "title")
            .name("Some Game (USA) (Track 1).bin", "component 1");
        let out = scrub(
            "FILE \"Some Game (USA) (Track 1).bin\" BINARY for Some Game (USA)",
            &context,
        );
        assert_eq!(out.text, "FILE \"<component-1>\" BINARY for <title>");
        assert_eq!(removed(&out, Removed::Name), 2);
    }

    #[test]
    fn a_name_inside_a_path_does_not_survive() {
        let context = ScrubContext::new().name("Some Game (USA).bin", "component 1");
        let out = scrub("read /staging/abc/Some Game (USA).bin failed", &context);
        assert!(!out.text.contains("Some Game"), "{}", out.text);
        assert!(!out.text.contains("staging"), "{}", out.text);
    }

    #[test]
    fn short_and_repeated_names_are_ignored() {
        let context = ScrubContext::new()
            .name("ab", "x")
            .name("disc", "one")
            .name("disc", "two");
        assert_eq!(scrub("ab disc", &context).text, "ab <one>");
    }

    #[test]
    fn labels_are_reduced_to_safe_characters() {
        assert_eq!(label_for("component 2"), "<component-2>");
        assert_eq!(label_for("  <weird>/label!! "), "<weirdlabel>");
        assert_eq!(label_for("!!!"), "<name>");
        assert!(label_for(&"x".repeat(100)).len() <= MAX_LABEL_CHARS + 3);
    }

    #[test]
    fn the_first_serial_key_with_a_value_wins() {
        // Found by the fuzzer: the empty `Drive id:` was found first and the
        // `Serial=` before it, which had a value, was never looked at.
        assert_eq!(plain("Serial=/Drive id:").text, "Serial= <serial>");
    }

    #[test]
    fn serial_numbers_are_removed() {
        let out = plain(
            "Drive type   : vendor 'Slimtype' product 'DVD A' revision 'KS21'\nDrive id     : 'K3DD12345678   '\nSerial number = 0042\n",
        );
        assert_eq!(
            out.text,
            "Drive type   : vendor 'Slimtype' product 'DVD A' revision 'KS21'\nDrive id     : <serial>\nSerial number = <serial>\n"
        );
        assert_eq!(removed(&out, Removed::Serial), 2);
        assert_eq!(plain("serialize the plan").text, "serialize the plan");
    }

    #[test]
    fn addresses_and_private_hosts_are_removed() {
        let out =
            plain("connect 192.168.1.13:5432 or [fe80::1%eth0] or nas.lan:8080, then 10.0.0.5.");
        assert_eq!(
            out.text,
            "connect <address>:5432 or [<address>] or <host>:8080, then <address>."
        );
        assert_eq!(removed(&out, Removed::Address), 3);
        assert_eq!(removed(&out, Removed::Host), 1);
    }

    #[test]
    fn a_path_behind_a_private_host_does_not_survive() {
        // Found by the fuzzer: the host was replaced after the path step had
        // passed over it, leaving the file name behind the placeholder.
        assert_eq!(
            plain("open nas.lan/share/Some Game.iso").text,
            "open <host><path>.iso"
        );
        assert_eq!(
            plain("from 10.0.0.5/discs/one.cue").text,
            "from <address><path>.cue"
        );
    }

    #[test]
    fn things_that_look_like_addresses_but_are_not_stay() {
        for kept in [
            "at 04:25:21 UTC",
            "Self::new and ArtifactFormat::Unknown",
            "version 1.2.3 and 1.2.3.256",
            "length 00:04:00",
            "mac aa:bb:cc:dd:ee:ff",
            "tangible.example.org",
            "localhost:8080",
        ] {
            assert_eq!(plain(kept).text, kept, "{kept}");
        }
    }

    #[test]
    fn addresses_can_be_kept() {
        let context = ScrubContext::new().scrub_addresses(false);
        assert_eq!(scrub("at 192.168.1.13", &context).text, "at 192.168.1.13");
    }

    #[test]
    fn input_is_bounded() {
        let long = "é".repeat(MAX_INPUT_BYTES);
        let out = plain(&long);
        assert!(out.truncated);
        assert!(out.text.len() <= MAX_INPUT_BYTES);
        assert!(!plain("short").truncated);
    }

    #[test]
    fn output_is_bounded_when_labels_are_longer_than_names() {
        let context = ScrubContext::new().name("abc", &"l".repeat(40));
        let out = scrub(&"abc ".repeat(MAX_INPUT_BYTES / 4), &context);
        assert!(out.truncated);
        assert!(out.text.len() <= MAX_OUTPUT_BYTES);
    }

    #[test]
    fn real_xorriso_output_loses_its_serial_and_keeps_the_drive() {
        let fixture = include_str!("../../../fixtures/tool-output/xorriso/toc-blank-cdr.txt");
        let out = plain(fixture);
        assert!(out.text.contains("Drive id     : <serial>"), "{}", out.text);
        assert!(out.text.contains("Slimtype"));
    }

    #[test]
    fn plain_text_is_untouched() {
        let text = "Burn failed: PREFLIGHT_MEDIUM_NOT_BLANK, the disc already has data.\n";
        let out = plain(text);
        assert_eq!(out.text, text);
        assert!(out.removed.is_empty());
    }
}

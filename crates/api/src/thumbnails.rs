// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Thumbnails beside a metadata search's results (ADR 0027).
//!
//! A search's candidates name where their providers keep their pictures.
//! The search holds each picture's address here, under an identifier that
//! says nothing of it, and answers the identifier. A page asks Tangible for
//! the thumbnail by it, never the provider: the first ask fetches it as a
//! cover is fetched (ADR 0023: through the metadata providers' outbound
//! client, public addresses only, at most 2 MiB, kept only when its leading
//! bytes are a JPEG's, a PNG's or a WebP's), and later asks are answered
//! with what was fetched. Each is held for an hour after the last search
//! that named it, in memory only, and a restart forgets them all.
//!
//! Only an address a search built can be fetched: an identifier names one,
//! and is not one. Each is fetched at most once for each search that names
//! it, however often it is asked for.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};
use tangible_integrations::artwork::{ArtworkError, Image, fetch_image};
use tangible_integrations::metadata::Picture;
use tangible_integrations::outbound::Url;
use tokio::sync::OnceCell;

use crate::metadata::Metadata;

/// How long a thumbnail is held after the last search that named it.
pub const HOLD_FOR: Duration = Duration::from_hours(1);

/// Most thumbnails held: four searches' worth from every source at once
/// is about 400.
const MOST_HELD: usize = 2048;

/// Most bytes of fetched thumbnails held. Most are 5 to 40 KiB; libretro's
/// box art, which comes in one size, can be a few hundred.
const MOST_BYTES: usize = 64 * 1024 * 1024;

/// The characters of an identifier: hexadecimal, 128 bits of a digest.
const ID_CHARS: usize = 32;

/// The thumbnails held. Every clone shares them.
#[derive(Clone)]
pub struct Thumbnails {
    metadata: Metadata,
    /// Mixed into every identifier, so that one cannot be made from an
    /// address to learn whether anybody searched for it.
    salt: [u8; 32],
    held: Arc<Mutex<HashMap<String, Held>>>,
}

impl std::fmt::Debug for Thumbnails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Thumbnails").finish_non_exhaustive()
    }
}

struct Held {
    url: Url,
    /// When a search last named it.
    named_at: Instant,
    fetched: Arc<OnceCell<Fetched>>,
}

impl Held {
    fn new(url: Url, now: Instant) -> Self {
        Self {
            url,
            named_at: now,
            fetched: Arc::new(OnceCell::new()),
        }
    }

    fn bytes(&self) -> usize {
        match self.fetched.get() {
            Some(Fetched::Image(image)) => image.bytes.len(),
            _ => 0,
        }
    }
}

#[derive(Debug, Clone)]
enum Fetched {
    Image(Arc<Image>),
    /// The provider has none: it answered 404.
    Missing,
    /// It could not be fetched; the next search that names it tries again.
    Failed,
}

impl Thumbnails {
    /// Thumbnails fetched through `metadata`'s client, from where its
    /// providers keep their images.
    ///
    /// # Errors
    ///
    /// None in practice: the system's randomness, for the salt, failing.
    pub fn new(metadata: Metadata) -> Result<Self, getrandom::Error> {
        let mut salt = [0_u8; 32];
        getrandom::fill(&mut salt)?;
        Ok(Self {
            metadata,
            salt,
            held: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn id_of(&self, url: &Url) -> String {
        let mut digest = Sha256::new();
        digest.update(self.salt);
        digest.update(url.as_str().as_bytes());
        let mut id = hex::encode(digest.finalize());
        id.truncate(ID_CHARS);
        id
    }

    /// Hold the thumbnail of each of `pictures`, and answer its identifier,
    /// in order: none for no picture, or one no address could be built for.
    /// A thumbnail held already is held for another hour, and one that
    /// failed is tried again.
    pub fn hold<'a, I>(&self, pictures: I) -> Vec<Option<String>>
    where
        I: IntoIterator<Item = Option<&'a Picture>>,
    {
        let endpoints = self.metadata.artwork_endpoints();
        let now = Instant::now();
        let urls: Vec<Option<Url>> = pictures
            .into_iter()
            .map(|picture| endpoints.thumbnail(picture?))
            .collect();
        let Ok(mut held) = self.held.lock() else {
            return urls.iter().map(|_| None).collect();
        };
        let ids = urls
            .into_iter()
            .map(|url| {
                let url = url?;
                let id = self.id_of(&url);
                match held.get_mut(&id) {
                    Some(entry) => {
                        entry.named_at = now;
                        if matches!(entry.fetched.get(), Some(Fetched::Failed)) {
                            entry.fetched = Arc::new(OnceCell::new());
                        }
                    }
                    None => {
                        held.insert(id.clone(), Held::new(url, now));
                    }
                }
                Some(id)
            })
            .collect();
        trim(&mut held, now);
        ids
    }

    /// The thumbnail held as `id`, fetched now if it has not been. None for
    /// an identifier not held, or held past its hour, and for a picture the
    /// provider has not got or that could not be fetched.
    pub async fn image(&self, id: &str) -> Option<Arc<Image>> {
        if id.len() != ID_CHARS {
            return None;
        }
        let (url, fetched) = {
            let held = self.held.lock().ok()?;
            let entry = held.get(id)?;
            if entry.named_at.elapsed() >= HOLD_FOR {
                return None;
            }
            (entry.url.clone(), Arc::clone(&entry.fetched))
        };
        let outcome = fetched
            .get_or_init(|| async {
                match fetch_image(self.metadata.http(), &url).await {
                    Ok(image) => Fetched::Image(Arc::new(image)),
                    Err(ArtworkError::Missing) => Fetched::Missing,
                    Err(ArtworkError::Failed(why)) => {
                        tracing::info!(host = url.host_str(), %why, "a thumbnail could not be fetched");
                        Fetched::Failed
                    }
                }
            })
            .await
            .clone();
        if let Ok(mut held) = self.held.lock() {
            trim(&mut held, Instant::now());
        }
        match outcome {
            Fetched::Image(image) => Some(image),
            Fetched::Missing | Fetched::Failed => None,
        }
    }
}

/// Let go of what is past its hour, then of the longest unnamed until no
/// more than [`MOST_HELD`] are held and their images take no more than
/// [`MOST_BYTES`]. A page still showing one let go shows the drawing that
/// stands in for a missing thumbnail.
fn trim(held: &mut HashMap<String, Held>, now: Instant) {
    held.retain(|_, entry| now.duration_since(entry.named_at) < HOLD_FOR);
    let mut bytes: usize = held.values().map(Held::bytes).sum();
    if held.len() <= MOST_HELD && bytes <= MOST_BYTES {
        return;
    }
    let mut oldest: Vec<(Instant, String)> = held
        .iter()
        .map(|(id, entry)| (entry.named_at, id.clone()))
        .collect();
    oldest.sort();
    for (_, id) in oldest {
        if held.len() <= MOST_HELD && bytes <= MOST_BYTES {
            break;
        }
        if let Some(entry) = held.remove(&id) {
            bytes = bytes.saturating_sub(entry.bytes());
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::metadata::Endpoints;
    use tangible_integrations::artwork::ImageType;
    use tangible_integrations::outbound::{OutboundClient, OutboundPolicy};

    fn thumbnails() -> Thumbnails {
        let client = OutboundClient::new(
            OutboundPolicy::for_integrations(Vec::new(), None),
            crate::metadata::USER_AGENT,
        )
        .unwrap();
        Thumbnails::new(Metadata::new(client, Endpoints::public())).unwrap()
    }

    fn poster(n: usize) -> Picture {
        Picture::TmdbPoster(format!("/p{n}.jpg"))
    }

    fn image(bytes: usize) -> Fetched {
        Fetched::Image(Arc::new(Image {
            bytes: vec![0; bytes],
            image_type: ImageType::Jpeg,
        }))
    }

    #[test]
    fn a_picture_is_held_under_an_identifier_that_is_not_its_address() {
        let thumbnails = thumbnails();
        let pictures = [
            Some(poster(1)),
            None,
            Some(Picture::IgdbImage("../x".into())),
            Some(poster(1)),
            Some(poster(2)),
        ];
        let ids = thumbnails.hold(pictures.iter().map(Option::as_ref));
        assert_eq!(ids.len(), 5);
        let first = ids[0].clone().unwrap();
        assert_eq!(first.len(), ID_CHARS);
        assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(ids[1], None, "no picture");
        assert_eq!(ids[2], None, "no address could be built");
        assert_eq!(
            ids[3].as_deref(),
            Some(first.as_str()),
            "one picture, one id"
        );
        assert_ne!(ids[4].as_deref(), Some(first.as_str()));
        assert_eq!(thumbnails.held.lock().unwrap().len(), 2);
        // Another server, or this one after a restart, makes other ids.
        let again = self::thumbnails().hold([Some(&poster(1))]);
        assert_ne!(again[0].as_deref(), Some(first.as_str()));
    }

    #[tokio::test]
    async fn an_identifier_not_held_or_past_its_hour_is_no_thumbnail() {
        let thumbnails = thumbnails();
        assert!(
            thumbnails
                .image("0123456789abcdef0123456789abcdef")
                .await
                .is_none()
        );
        assert!(thumbnails.image("../../etc/passwd").await.is_none());
        let id = thumbnails.hold([Some(&poster(1))])[0].clone().unwrap();
        {
            let mut held = thumbnails.held.lock().unwrap();
            let entry = held.get_mut(&id).unwrap();
            entry.fetched = Arc::new(OnceCell::new_with(Some(image(10))));
        }
        assert_eq!(thumbnails.image(&id).await.unwrap().bytes.len(), 10);
        {
            let mut held = thumbnails.held.lock().unwrap();
            let entry = held.get_mut(&id).unwrap();
            entry.named_at = Instant::now().checked_sub(HOLD_FOR).unwrap();
        }
        assert!(thumbnails.image(&id).await.is_none(), "past its hour");
    }

    #[test]
    fn a_search_that_names_one_again_holds_it_again_and_retries_a_failure() {
        let thumbnails = thumbnails();
        let ids = thumbnails.hold([Some(&poster(1)), Some(&poster(2))]);
        let (failed, missing) = (ids[0].clone().unwrap(), ids[1].clone().unwrap());
        let long_ago = Instant::now().checked_sub(Duration::from_mins(50)).unwrap();
        {
            let mut held = thumbnails.held.lock().unwrap();
            for (id, outcome) in [(&failed, Fetched::Failed), (&missing, Fetched::Missing)] {
                let entry = held.get_mut(id).unwrap();
                entry.fetched = Arc::new(OnceCell::new_with(Some(outcome)));
                entry.named_at = long_ago;
            }
        }
        thumbnails.hold([Some(&poster(1)), Some(&poster(2))]);
        let held = thumbnails.held.lock().unwrap();
        assert!(held[&failed].fetched.get().is_none(), "tried again");
        assert!(matches!(
            held[&missing].fetched.get(),
            Some(Fetched::Missing)
        ));
        assert!(held.values().all(|entry| entry.named_at > long_ago));
    }

    #[test]
    fn the_longest_unnamed_are_let_go_past_the_bounds() {
        let now = Instant::now();
        let url =
            |n: usize| Url::parse(&format!("https://image.tmdb.org/t/p/w154/{n}.jpg")).unwrap();
        let mut held = HashMap::new();
        // Named a second apart and within the hour, the first longest ago.
        let ago = |n: usize| Duration::from_millis(3_000_000 - 1_000 * n as u64);
        for n in 0..MOST_HELD + 10 {
            let named_at = now.checked_sub(ago(n)).unwrap();
            held.insert(format!("{n}"), Held::new(url(n), named_at));
        }
        // And one past its hour, which goes first.
        held.insert(
            "old".into(),
            Held::new(url(99_999), now.checked_sub(HOLD_FOR).unwrap()),
        );
        trim(&mut held, now);
        assert_eq!(held.len(), MOST_HELD);
        assert!(!held.contains_key("old"));
        for n in 0..10 {
            assert!(!held.contains_key(&format!("{n}")), "the oldest go");
        }
        assert!(held.contains_key(&format!("{}", MOST_HELD + 9)));

        // Bytes: two images of half the bound and one more are too many.
        let mut held = HashMap::new();
        for (n, bytes) in [(0, MOST_BYTES / 2), (1, MOST_BYTES / 2), (2, 1024)] {
            let mut entry = Held::new(
                url(n),
                now.checked_sub(Duration::from_secs(100 - n as u64))
                    .unwrap(),
            );
            entry.fetched = Arc::new(OnceCell::new_with(Some(image(bytes))));
            held.insert(format!("{n}"), entry);
        }
        trim(&mut held, now);
        let mut left: Vec<_> = held.keys().cloned().collect();
        left.sort();
        assert_eq!(left, ["1", "2"]);
    }
}

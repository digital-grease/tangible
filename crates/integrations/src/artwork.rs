// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Cover art (ADR 0023): where each provider keeps an item's cover, and
//! fetching one as an image that is only an image.
//!
//! The server fetches a cover once and serves it itself; no browser asks a
//! provider. What comes back is untrusted: it is read up to
//! [`MAX_IMAGE_BYTES`], and kept only when its leading bytes are a JPEG's,
//! a PNG's or a WebP's, whatever the provider said it was. Nothing is
//! decoded here, and nothing else (an SVG, an HTML error page) is kept.

use std::time::Duration;

use crate::download::read_bytes_within;
use crate::metadata::Picture;
use crate::outbound::{OutboundClient, OutboundError, Url};

/// The largest cover kept.
pub const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;

/// How long one cover may take.
const TIMEOUT: Duration = Duration::from_secs(30);

const CLIENT: &str = "cover art";

/// Where the providers keep their images.
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// TMDB's image service, sizes under it.
    pub tmdb_images: Url,
    /// The Cover Art Archive, MusicBrainz's covers.
    pub cover_art_archive: Url,
    /// IGDB's image service.
    pub igdb_images: Url,
    /// libretro's thumbnail server.
    pub libretro: Url,
}

impl Endpoints {
    /// The providers' own addresses.
    ///
    /// # Panics
    ///
    /// Never: the addresses are constants that parse.
    #[must_use]
    #[allow(clippy::expect_used)]
    pub fn public() -> Self {
        let url = |text: &str| Url::parse(text).expect("a constant address parses");
        Self {
            tmdb_images: url("https://image.tmdb.org/t/p/"),
            cover_art_archive: url("https://coverartarchive.org/"),
            igdb_images: url("https://images.igdb.com/igdb/image/upload/"),
            libretro: url("https://thumbnails.libretro.com/"),
        }
    }

    /// TMDB's poster at `path` (`/abc.jpg`), 342 pixels wide.
    #[must_use]
    pub fn tmdb_poster(&self, path: &str) -> Option<Url> {
        self.tmdb_sized(path, "w342")
    }

    fn tmdb_sized(&self, path: &str, size: &str) -> Option<Url> {
        let name = path.strip_prefix('/')?;
        self.tmdb_images.join(&format!("{size}/{name}")).ok()
    }

    /// The front cover of the MusicBrainz release `release`, 500 pixels.
    #[must_use]
    pub fn release_front(&self, release: &str) -> Option<Url> {
        self.release_sized(release, 500)
    }

    fn release_sized(&self, release: &str, size: u16) -> Option<Url> {
        let id = mbid(release)?;
        self.cover_art_archive
            .join(&format!("release/{id}/front-{size}"))
            .ok()
    }

    /// A search result's picture as a thumbnail (ADR 0027), the smallest
    /// each provider keeps that is sharp at twice the size it is shown:
    /// TMDB's poster 154 pixels wide, the Cover Art Archive's front at 250,
    /// IGDB's small cover at twice its size, and libretro's box art as it is,
    /// since libretro keeps one size.
    #[must_use]
    pub fn thumbnail(&self, picture: &Picture) -> Option<Url> {
        match picture {
            Picture::TmdbPoster(path) => self.tmdb_sized(path, "w154"),
            Picture::Release(release) => self.release_sized(release, 250),
            Picture::IgdbImage(image_id) => self.igdb_sized(image_id, "t_cover_small_2x"),
            Picture::Libretro(key) => self.libretro_box_art(key),
        }
    }

    /// libretro's box art for the key [`libretro_key`] made:
    /// `<system>/Named_Boxarts/<name>.png`, each part percent-encoded.
    #[must_use]
    pub fn libretro_box_art(&self, key: &str) -> Option<Url> {
        let (system, name) = key.split_once('/')?;
        if system.is_empty() || name.is_empty() {
            return None;
        }
        let mut url = self.libretro.clone();
        url.path_segments_mut()
            .ok()?
            .pop_if_empty()
            .push(system)
            .push("Named_Boxarts")
            .push(&format!("{name}.png"));
        Some(url)
    }

    /// IGDB's cover image `image_id`, at its cover size.
    #[must_use]
    pub fn igdb_cover(&self, image_id: &str) -> Option<Url> {
        self.igdb_sized(image_id, "t_cover_big")
    }

    fn igdb_sized(&self, image_id: &str, size: &str) -> Option<Url> {
        if image_id.is_empty() || !image_id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        self.igdb_images
            .join(&format!("{size}/{image_id}.jpg"))
            .ok()
    }
}

/// A game's name as libretro names its thumbnail file: each of
/// `&*/:\`<>?\|"` replaced by `_`, as libretro's thumbnail repositories do.
#[must_use]
pub fn libretro_name(game: &str) -> String {
    game.chars()
        .map(|c| if "&*/:`<>?\\|\"".contains(c) { '_' } else { c })
        .collect()
}

/// The artwork key for a DAT game's box art: its DAT's name (libretro's
/// system, such as `Sony - PlayStation`), `/`, and the game's libretro
/// name. None when either is empty, carries a control character, or the
/// key would be over 500 characters.
#[must_use]
pub fn libretro_key(system: &str, game: &str) -> Option<String> {
    let system = system.trim();
    let name = libretro_name(game.trim());
    let clean = |text: &str| !text.is_empty() && !text.chars().any(char::is_control);
    if !clean(system) || system.contains('/') || !clean(&name) {
        return None;
    }
    let key = format!("{system}/{name}");
    (key.chars().count() <= 500).then_some(key)
}

/// A MusicBrainz id, lowercased: 36 characters, hex digits with hyphens
/// after the 8th, 12th, 16th and 20th.
fn mbid(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let shaped = lower.len() == 36
        && lower.bytes().enumerate().all(|(at, b)| {
            if matches!(at, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        });
    shaped.then_some(lower)
}

/// What an image is, by its leading bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageType {
    /// A JPEG.
    Jpeg,
    /// A PNG.
    Png,
    /// A WebP.
    Webp,
}

impl ImageType {
    /// Its media type.
    #[must_use]
    pub const fn media_type(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Webp => "image/webp",
        }
    }

    /// The type a media type names, of these.
    #[must_use]
    pub fn from_media_type(media_type: &str) -> Option<Self> {
        match media_type {
            "image/jpeg" => Some(Self::Jpeg),
            "image/png" => Some(Self::Png),
            "image/webp" => Some(Self::Webp),
            _ => None,
        }
    }
}

/// What `bytes` is, if it starts as a JPEG, a PNG or a WebP does.
#[must_use]
pub fn image_type(bytes: &[u8]) -> Option<ImageType> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(ImageType::Jpeg)
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageType::Png)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(ImageType::Webp)
    } else {
        None
    }
}

/// A cover, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Its bytes, as fetched.
    pub bytes: Vec<u8>,
    /// What its leading bytes say it is.
    pub image_type: ImageType,
}

/// Why a cover was not fetched.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArtworkError {
    /// The provider has none: it answered 404.
    #[error("the provider has no such image")]
    Missing,
    /// Anything else, said in words.
    #[error("{0}")]
    Failed(String),
}

/// Fetch the image at `url`, and keep it only if it is one.
///
/// # Errors
///
/// [`ArtworkError::Missing`] for a 404, [`ArtworkError::Failed`] for
/// anything else: a refused address, no answer, a body over
/// [`MAX_IMAGE_BYTES`], or bytes that are not a JPEG, PNG or WebP.
pub async fn fetch_image(http: &OutboundClient, url: &Url) -> Result<Image, ArtworkError> {
    let failed = |detail: String| ArtworkError::Failed(detail);
    let request = http
        .get(url)
        .map_err(|error: OutboundError| failed(error.to_string()))?
        .header(reqwest::header::ACCEPT, "image/jpeg, image/png, image/webp")
        .timeout(TIMEOUT);
    let response = http
        .send(request)
        .await
        .map_err(|error| failed(error.to_string()))?;
    match response.status().as_u16() {
        200 => {}
        404 => return Err(ArtworkError::Missing),
        code => return Err(failed(format!("the image server answered {code}"))),
    }
    let bytes = read_bytes_within(CLIENT, response, MAX_IMAGE_BYTES)
        .await
        .map_err(|error| failed(error.to_string()))?;
    let image_type = image_type(&bytes)
        .ok_or_else(|| failed("the answer was not a JPEG, PNG or WebP".into()))?;
    Ok(Image { bytes, image_type })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn an_image_is_known_by_its_leading_bytes_only() {
        assert_eq!(
            image_type(&[0xFF, 0xD8, 0xFF, 0xE0, 0]),
            Some(ImageType::Jpeg)
        );
        assert_eq!(image_type(b"\x89PNG\r\n\x1a\n\0\0"), Some(ImageType::Png));
        assert_eq!(image_type(b"RIFF\x10\0\0\0WEBPVP8 "), Some(ImageType::Webp));
        for not in [
            &b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"[..],
            b"<!doctype html><html>",
            b"GIF89a",
            b"RIFF\x10\0\0\0WAVEfmt ",
            b"",
            &[0xFF, 0xD8][..],
        ] {
            assert_eq!(image_type(not), None, "{not:?}");
        }
        assert_eq!(ImageType::Webp.media_type(), "image/webp");
        assert_eq!(
            ImageType::from_media_type("image/png"),
            Some(ImageType::Png)
        );
        assert_eq!(ImageType::from_media_type("image/svg+xml"), None);
    }

    #[test]
    fn each_provider_keeps_its_covers_where_it_says() {
        let at = Endpoints::public();
        assert_eq!(
            at.tmdb_poster("/f89U3ADr1oiB1s9GkdPOEpXUk5H.jpg")
                .unwrap()
                .as_str(),
            "https://image.tmdb.org/t/p/w342/f89U3ADr1oiB1s9GkdPOEpXUk5H.jpg"
        );
        assert!(at.tmdb_poster("no-slash.jpg").is_none());
        assert_eq!(
            at.release_front("76DF3287-6CDA-33EB-8E9A-044B5E15FFDD")
                .unwrap()
                .as_str(),
            "https://coverartarchive.org/release/76df3287-6cda-33eb-8e9a-044b5e15ffdd/front-500"
        );
        assert!(at.release_front("not-an-mbid").is_none());
        assert_eq!(
            at.igdb_cover("co1wyy").unwrap().as_str(),
            "https://images.igdb.com/igdb/image/upload/t_cover_big/co1wyy.jpg"
        );
        assert!(at.igdb_cover("../x").is_none());
        assert!(at.igdb_cover("").is_none());
    }

    #[test]
    fn a_thumbnail_is_each_provider_s_small_size() {
        let at = Endpoints::public();
        let thumbnail = |picture: Picture| at.thumbnail(&picture).map(|url| url.to_string());
        assert_eq!(
            thumbnail(Picture::TmdbPoster(
                "/f89U3ADr1oiB1s9GkdPOEpXUk5H.jpg".into()
            ))
            .as_deref(),
            Some("https://image.tmdb.org/t/p/w154/f89U3ADr1oiB1s9GkdPOEpXUk5H.jpg")
        );
        assert_eq!(
            thumbnail(Picture::Release(
                "76df3287-6cda-33eb-8e9a-044b5e15ffdd".into()
            ))
            .as_deref(),
            Some(
                "https://coverartarchive.org/release/76df3287-6cda-33eb-8e9a-044b5e15ffdd/front-250"
            )
        );
        assert_eq!(
            thumbnail(Picture::IgdbImage("co1wyy".into())).as_deref(),
            Some("https://images.igdb.com/igdb/image/upload/t_cover_small_2x/co1wyy.jpg")
        );
        assert_eq!(
            thumbnail(Picture::Libretro(
                "Sony - PlayStation/Crash Bandicoot (USA)".into()
            ))
            .as_deref(),
            Some(
                "https://thumbnails.libretro.com/Sony%20-%20PlayStation/Named_Boxarts/Crash%20Bandicoot%20(USA).png"
            )
        );
        // Each is checked as a cover's is.
        for picture in [
            Picture::TmdbPoster("no-slash.jpg".into()),
            Picture::Release("not-an-mbid".into()),
            Picture::IgdbImage("../x".into()),
            Picture::Libretro("no system".into()),
        ] {
            assert_eq!(at.thumbnail(&picture), None, "{picture:?}");
        }
    }

    #[test]
    fn a_dat_game_has_its_box_art_where_libretro_keeps_it() {
        assert_eq!(
            libretro_name("Tom & Jerry: Fists of Furry? <USA> \"Rev 1\" a/b\\c|d*e`f"),
            "Tom _ Jerry_ Fists of Furry_ _USA_ _Rev 1_ a_b_c_d_e_f"
        );
        let key = libretro_key("Sony - PlayStation", "Crash Bandicoot (USA)").unwrap();
        assert_eq!(key, "Sony - PlayStation/Crash Bandicoot (USA)");
        assert_eq!(
            Endpoints::public().libretro_box_art(&key).unwrap().as_str(),
            "https://thumbnails.libretro.com/Sony%20-%20PlayStation/Named_Boxarts/Crash%20Bandicoot%20(USA).png"
        );
        // A name's own slash cannot become a path.
        let slashed = libretro_key("Nintendo - Game Boy", "A/B (World)").unwrap();
        assert!(
            Endpoints::public()
                .libretro_box_art(&slashed)
                .unwrap()
                .as_str()
                .ends_with("/Named_Boxarts/A_B%20(World).png")
        );
        for (system, game) in [("", "x"), ("Sony", ""), ("A/B", "x"), ("Sony", "x\ny")] {
            assert_eq!(libretro_key(system, game), None, "{system:?} {game:?}");
        }
        assert_eq!(libretro_key("S", &"x".repeat(600)), None);
    }
}

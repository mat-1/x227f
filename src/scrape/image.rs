use std::{collections::HashMap, io::Cursor, sync::Arc, time::Instant};

use base64::Engine;
use eyre::{bail, eyre};
use futures_util::StreamExt;
use image::{codecs::gif, AnimationDecoder, ImageFormat};
use parking_lot::RwLock;
use sha2::{Digest, Sha256};
use tracing::{debug, error, instrument, trace, warn};
use url::Url;

use crate::{
    data::{ButtonData, CachedButton, RedirectSource},
    RECRAWL_BUTTONS_INTERVAL_HOURS,
};

use super::{page::CandidateButton, ScrapeContext};

pub async fn scrape_images(
    ctx: &ScrapeContext,
    candidates: Vec<CandidateButton>,
    button_cache: &Arc<RwLock<HashMap<Url, CachedButton>>>,
) -> Vec<ButtonData> {
    let mut results = vec![None; candidates.len()];

    let mut valid_88x31_count = 0_usize;
    let mut scraped_image_count = 0_usize;

    let mut candidates_shuffled = candidates.into_iter().enumerate().collect::<Vec<_>>();
    // if there's more than 100 candidates, then move the last 50 to the beginning
    // (since 88x31s will more often be near the end or beginning of the html)
    if candidates_shuffled.len() > 100 {
        candidates_shuffled.rotate_right(50);
    }
    for (i, candidate) in candidates_shuffled {
        let image_url = candidate.img.url.clone();
        scraped_image_count += 1;
        match scrape_image(ctx, candidate, button_cache).await {
            Ok(Some(button_data)) => {
                results[i] = Some(button_data);
                valid_88x31_count += 1;
            }
            Ok(None) => {
                // wasn't an 88x31, oh well
            }
            Err(e) => {
                warn!("failed to scrape image {image_url}: {e}");
            }
        }

        if scraped_image_count > 100 && valid_88x31_count == 0 {
            warn!("we've scraped 100 images and none of them were 88x31, aborting");
            break;
        }
    }

    results.into_iter().flatten().collect()
}

#[instrument(skip_all, fields(url = %candidate.img.url))]
pub async fn scrape_image(
    ctx: &ScrapeContext,
    candidate: CandidateButton,
    button_cache: &Arc<RwLock<HashMap<Url, CachedButton>>>,
) -> eyre::Result<Option<ButtonData>> {
    let image_url = transform_image_url_to_clean_up(candidate.img.url);

    let cached_button = button_cache.read().get(&image_url).cloned();
    if let Some(cached_button) = &cached_button {
        // if last_visited is within RECRAWL_INTERVAL_HOURS
        if cached_button.last_visited
            + chrono::Duration::hours(RECRAWL_BUTTONS_INTERVAL_HOURS as i64)
            > chrono::Utc::now()
        {
            trace!("using cache for {image_url}");
            return Ok(Some(ButtonData {
                source: Some(image_url),
                hash: cached_button.hash.clone(),
                file_ext: cached_button.file_ext.clone(),
                target: candidate.href,
                last_visited: cached_button.last_visited,
                redirect: None,
                alt: candidate.img.alt,
                title: candidate.img.title,
            }));
        } else {
            let hours_ago: f64 =
                (chrono::Utc::now() - cached_button.last_visited).num_minutes() as f64 / 60.0;
            trace!("image is in cache but cache is too old (last visit was {hours_ago} hours ago)",);
        }
    }

    let DownloadImageResult {
        bytes,
        format,
        url,
        redirect,
    } = match download_88x31_image(ctx, image_url.clone()).await {
        Ok(res) => res,
        Err(e) => {
            // if it errored then check if it's cached
            if let Some(cached_button) = cached_button {
                debug!("failed to download image {image_url}, using cache: {e}");
                return Ok(Some(ButtonData {
                    source: Some(image_url),
                    hash: cached_button.hash,
                    file_ext: cached_button.file_ext,
                    target: candidate.href,
                    last_visited: cached_button.last_visited,
                    redirect: None,
                    alt: candidate.img.alt,
                    title: candidate.img.title,
                }));
            } else {
                return Err(e);
            }
        }
    };

    if bytes.is_empty() {
        return Ok(None);
    }
    let Some(format) = format else {
        return Ok(None);
    };

    // validate image size (potentially redundantly, since we sometimes validate the
    // first chunk of the response in download_88x31_image, but it's fine)
    match validate_image_size(&bytes, format) {
        Some(false) => {
            // not 88x31
            return Ok(None);
        }
        Some(true) => {}
        None => {
            trace!("Bytes: {bytes:?}");
            trace!("couldn't determine image size, aborting");
            return Ok(None);
        }
    }

    // compress/re-encode, hash, and save

    let bytes = re_encode_image(bytes, format)?;
    if bytes.is_empty() {
        // this happens if the image wasn't 88x31
        return Ok(None);
    }

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let hash = hex::encode(hasher.finalize());
    // truncate to 32 bytes because big file names are ugly
    // pretty sure we won't get collisions anyways
    let hash = hash[..32].to_owned();

    // save to data/buttons/<sha256>.<ext>
    let file_ext = format.extensions_str()[0];
    let file_name = format!("{hash}.{file_ext}");
    let file_path = std::path::Path::new("data/buttons").join(file_name);
    // only write if it doesn't exist
    if !file_path.exists() {
        tokio::fs::write(file_path, bytes).await?;
    }

    let button_data = ButtonData {
        source: url,
        hash,
        file_ext: file_ext.to_owned(),
        target: candidate.href,
        last_visited: chrono::Utc::now(),
        redirect,
        alt: candidate.img.alt,
        title: candidate.img.title,
    };

    trace!("scraped image: {button_data:?}");

    Ok(Some(button_data))
}

fn re_encode_image(bytes: Vec<u8>, format: image::ImageFormat) -> eyre::Result<Vec<u8>> {
    let start = Instant::now();
    let bytes = match format {
        // for most we just fall back to image's normal re-encoding
        ImageFormat::Png => {
            // for png we use oxipng because we get significantly smaller file
            // sizes

            let bytes = oxipng::optimize_from_memory(
                &bytes,
                &oxipng::Options {
                    strip: oxipng::StripChunks::Safe,
                    ..Default::default()
                },
            )
            .map_err(|e| eyre!("oxipng error: {e}"))?;

            // validate again JUST to be sure we weren't bamboozled and the image somehow
            // stopped being 88x31

            if !validate_image_size(&bytes, format).unwrap_or_default() {
                return Ok(vec![]);
            }

            bytes
        }
        ImageFormat::Gif => {
            // use GifDecoder because otherwise we lose the animation

            let decoder = gif::GifDecoder::new(Cursor::new(bytes))?;
            let frames = decoder.into_frames();

            let mut bytes = Vec::new();
            {
                let mut encoder = gif::GifEncoder::new(&mut bytes);
                // pretty much everyone uses infinite repeat
                encoder.set_repeat(gif::Repeat::Infinite)?;
                if let Err(_e) = encoder.try_encode_frames(frames) {
                    // this is usually fine, some gifs are weird and contain
                    // invalid data ?
                }
            }

            bytes
        }
        _ => {
            let img = image::load_from_memory_with_format(&bytes, format)?;

            let mut bytes = Cursor::new(Vec::new());
            img.write_to(&mut bytes, format)?;

            bytes.into_inner()
        }
    };

    let duration = start.elapsed();
    trace!("re-encoding image took {duration:?}");

    Ok(bytes)
}

struct DownloadImageResult {
    /// The bytes of the image. This may be empty if the image was not 88x31
    /// (but not always, you should check again just to make sure).
    bytes: Vec<u8>,
    format: Option<ImageFormat>,
    /// The final URL of the image after following redirects. This is None if
    /// the image was linked as a data: URI.
    url: Option<Url>,
    redirect: Option<RedirectSource>,
}

async fn download_88x31_image(ctx: &ScrapeContext, url: Url) -> eyre::Result<DownloadImageResult> {
    if url.scheme() == "data" {
        return parse_data_uri(url);
    }

    trace!("downloading image");
    let request_start = Instant::now();
    let requesting_url = transform_image_url_to_bypass_blocks(url.clone());
    let was_url_transformed = requesting_url != url;
    if was_url_transformed {
        trace!("image url was transformed to {requesting_url}");
    }
    let res = ctx.http.get(requesting_url.clone()).send().await?;

    let request_duration = request_start.elapsed();
    trace!("image request took {request_duration:?}");

    let res_url = res.url().clone();

    if !res.status().is_success() {
        bail!(
            "image response status was not success, status: {status}, requested url: {requesting_url}, final url: {res_url}",
            status = res.status()
        );
    }

    // only include redirects if we didn't transform the url
    let redirect = if res_url != requesting_url && !was_url_transformed {
        Some(RedirectSource {
            from: url.clone().into(),
            last_visited: chrono::Utc::now(),
        })
    } else {
        None
    };
    let image_url = if was_url_transformed { url } else { res_url };

    let res_headers = res.headers();

    // check to make sure the content-type starts with image/
    // we don't use this to determine the actual content type though
    let content_type = res_headers
        .get(reqwest::header::CONTENT_TYPE)
        .ok_or_else(|| {
            error!("missing content-type header, requested url: {requesting_url}, headers: {res_headers:?}");
            eyre!("missing content-type header")
        })?
        .to_str()?;
    if !content_type.starts_with("image/") {
        bail!(
            "image content-type was not image/, content-type: {content_type}, requested url: {requesting_url}, headers: {res_headers:?}",
            content_type = content_type,
        );
    }

    let mut bytes = Vec::new();
    let mut stream = res.bytes_stream();

    let mut checked_image_size_yet = false;

    let mut format = None;

    while let Some(Ok(chunk)) = stream.next().await {
        bytes.extend_from_slice(&chunk);
        // if it's more than 10mb then no thanks
        if bytes.len() > 10 * 1024 * 1024 {
            warn!("too much data sent for image, aborting");
            return Ok(DownloadImageResult {
                bytes: vec![],
                format: None,
                url: None,
                redirect,
            });
        }

        // only check after 1024 bytes in case the first chunk is really small or
        // something.
        // 1024 bytes is probably way more than needed but it doesn't really matter.
        if bytes.len() >= 1024 && !checked_image_size_yet {
            checked_image_size_yet = true;
            // this should most definitely not fail
            format = Some(image::guess_format(&bytes)?);
            // idk when it's possible for this to fail but in theory someone could put a
            // bunch of data before the part of the format that has the size. the 1024 check
            // exists to reduce the chances of this happening but it still could.
            if let Some(is_88x31) = validate_image_size(&bytes, format.unwrap()) {
                if !is_88x31 {
                    trace!("image was not 88x31, aborting");
                    return Ok(DownloadImageResult {
                        bytes: vec![],
                        format: None,
                        url: None,
                        redirect,
                    });
                }
            } else {
                trace!("tried to do early image size validation but couldn't");
            }
        }
    }

    if format.is_none() {
        format = Some(image::guess_format(&bytes)?);
        trace!("guessed image format: {format:?}", format = format.unwrap());
    }

    Ok(DownloadImageResult {
        bytes,
        format,
        url: Some(image_url),
        redirect,
    })
}

fn parse_data_uri(url: Url) -> eyre::Result<DownloadImageResult> {
    trace!("parsing data image");
    // data URIs are base64-encoded, so we can just decode them
    // data:[<mediatype>][;base64],<data>

    let (mediatype_and_encoding, data) = url
        .path()
        .split_once(',')
        .ok_or_else(|| eyre!("invalid data URI"))?;

    let (mediatype, encoding) = mediatype_and_encoding
        .rsplit_once(';')
        .unwrap_or((mediatype_and_encoding, ""));
    let mediatype = if mediatype.is_empty() {
        // this is the actual mediatype that browsers default to.
        // it doesn't actually matter because we're going to ignore it anyways but i
        // like it being correct.
        "text/plain;charset=US-ASCII"
    } else {
        mediatype
    };

    let data = percent_encoding::percent_decode_str(data).collect::<Vec<u8>>();
    let data = if encoding == "base64" {
        base64::engine::general_purpose::STANDARD.decode(data)?
    } else {
        data
    };
    Ok(DownloadImageResult {
        bytes: data,
        format: mimetype_to_format(mediatype),
        url: None,
        redirect: None,
    })
}

/// Transforms the given URL to bypass blocks on certain sites.
fn transform_image_url_to_bypass_blocks(mut url: Url) -> Url {
    if let Some(host) = url.host_str().map(|s| s.to_string()) {
        // imgur blocks most non-residential IPs, but we can bypass that by using
        // duckduckgo's image proxy
        if host == "i.imgur.com" || host == "imgur.com" {
            let Ok(new_url) =
                Url::parse_with_params("https://proxy.duckduckgo.com/iu/", &[("u", url.as_str())])
            else {
                error!("failed to parse ddg proxy imgur url: {url}");
                return url;
            };
            url = new_url;
        }

        // https://web.archive.org/web/0if_/http://i52.tinypic.com/334ne3n.png
        // http://img513.imageshack.us/img513/2479/anigif3n.gif
        // http://s24.postimg.org/jsqd1uxm9/mtnaff.jpg
        let is_dead_image_hosting_site = {
            ((host.starts_with('i') || host.starts_with("oi")) && host.ends_with(".tinypic.com"))
                || (host.starts_with("img") && host.ends_with(".imageshack.us"))
                || host.ends_with(".postimg.org")
                || host.ends_with(".nickpic.host")
        };
        if is_dead_image_hosting_site {
            // 0if_ makes it use the oldest saved version since a newer one might be a
            // redirect or an error
            let Ok(new_url) = Url::parse(&format!("https://web.archive.org/web/0if_/{url}")) else {
                error!("failed to parse archive.org tinypic url: {url}");
                return url;
            };
            url = new_url;
        }
    }

    url
}

/// Transforms the given image URL to remove unnecessary parts, like removing
/// /_next/image when possible.
fn transform_image_url_to_clean_up(mut url: Url) -> Url {
    // nextjs ruins images by making them webp and compressing them
    if url.path() == "/_next/image" {
        if let Some((_, url_param)) = url.query_pairs().find(|(k, _)| k == "url") {
            if let Ok(new_url) = url.join(&url_param) {
                url = new_url;
            }
        }
    }

    url
}

fn mimetype_to_format(mimetype: &str) -> Option<image::ImageFormat> {
    let mimetype = mimetype.split(';').next().unwrap_or(mimetype).trim();
    match mimetype {
        // `image` has its own from_mime_type function but i implemented my own in case i need to
        // add overrides and because we don't support every format `image` supports
        "image/png" => Some(ImageFormat::Png),
        "image/jpeg" | "image/jpg" => Some(ImageFormat::Jpeg),
        "image/gif" => Some(ImageFormat::Gif),
        "image/webp" => Some(ImageFormat::WebP),
        "image/avif" => Some(ImageFormat::Avif),
        "image/bmp" | "image/x-ms-bmp" => Some(ImageFormat::Bmp),
        _ => {
            if mimetype.starts_with("image/") {
                warn!("unsupported image format: {mimetype}");
            }
            None
        }
    }
}

/// Validates whether the headers of the image indicate that it's 88x31. This
/// returns `None` if it can't be determined from the given bytes.
fn validate_image_size(data: &[u8], format: image::ImageFormat) -> Option<bool> {
    let mut reader = image::io::Reader::new(Cursor::new(data));
    reader.set_format(format);

    let dimensions = reader.into_dimensions().ok()?;
    Some(dimensions == (88, 31))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_data_uri_gif() {
        let bytes = "data:image/gif;base64,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";

        let url = Url::parse(bytes).unwrap();
        let DownloadImageResult { bytes, format, .. } = parse_data_uri(url).unwrap();

        let _bytes = re_encode_image(bytes, format.unwrap()).unwrap();
    }
}

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Instant,
};

use compact_str::{CompactString, ToCompactString};
use eyre::{bail, eyre};
use futures_util::StreamExt;
use parking_lot::RwLock;
use tracing::{debug, instrument, warn};
use url::Url;

use crate::{
    data::{CachedButton, Page, RedirectSource},
    scrape,
};

use super::ScrapeContext;

/// Scrape the given page.
#[instrument(skip_all, fields(url = %res.url()))]
pub async fn scrape_page_from_download(
    ctx: &ScrapeContext,
    DownloadPageResult { res, redirect }: DownloadPageResult,
    button_cache: &Arc<RwLock<HashMap<Url, CachedButton>>>,
) -> eyre::Result<Page> {
    if !res.status().is_success() {
        // if it's an error page then don't scrape it
        return Ok(Page {
            url: res.url().clone(),
            last_visited: chrono::Utc::now(),
            failed: 0,
            buttons: vec![],
            other_internal_links: HashSet::default(),
            redirects: redirect.into_iter().collect(),
        });
    }

    let res_url = res.url().clone();

    // validate headers
    let res_headers = res.headers();
    let content_type = res_headers
        .get(reqwest::header::CONTENT_TYPE)
        .ok_or_else(|| eyre!("missing content-type header"))?
        .to_str()?;
    if !content_type.starts_with("text/html") {
        bail!("skipping non-html page: {content_type}");
    }

    let mut body_bytes = Vec::new();
    let mut body_bytes_stream = res.bytes_stream();
    while let Some(Ok(chunk)) = body_bytes_stream.next().await {
        body_bytes.extend_from_slice(&chunk);
        // if it's more than 10mb then no thanks
        if body_bytes.len() > 10 * 1024 * 1024 {
            warn!("too much data sent for page, aborting");
            return Ok(Page {
                url: res_url,
                last_visited: chrono::Utc::now(),
                failed: 0,
                buttons: vec![],
                other_internal_links: HashSet::default(),
                redirects: redirect.into_iter().collect(),
            });
        }
    }
    let body = String::from_utf8_lossy(&body_bytes);

    let CandidateLinks {
        candidate_buttons,
        other_internal_links,
    } = candidate_links_from_html(&body, &res_url);

    let request_images_start = Instant::now();
    let buttons = scrape::image::scrape_images(ctx, candidate_buttons, button_cache).await;
    let request_images_duration = request_images_start.elapsed();
    debug!("image requests took {request_images_duration:?}");

    Ok(Page {
        url: res_url,
        last_visited: chrono::Utc::now(),
        failed: 0,
        buttons,
        other_internal_links,
        redirects: redirect.into_iter().collect(),
    })
}

pub struct DownloadPageResult {
    pub res: reqwest::Response,
    pub redirect: Option<RedirectSource>,
}

#[instrument(skip_all, fields(url = %url))]
pub async fn download_page(ctx: &ScrapeContext, url: Url) -> eyre::Result<DownloadPageResult> {
    if url.host_str().iter().any(|host| {
        host.chars()
            .any(|c| !c.is_ascii_alphanumeric() && c != '-' && c != '.')
    }) {
        bail!("invalid host");
    }

    let request_start = Instant::now();
    let res = ctx.http.get(url.clone()).send().await?;
    let request_duration = request_start.elapsed();
    debug!("page request took {request_duration:?}");

    let res_url = res.url().clone();

    let redirect = if res_url != url {
        Some(RedirectSource {
            from: url.clone().into(),
            last_visited: chrono::Utc::now(),
        })
    } else {
        None
    };

    Ok(DownloadPageResult { res, redirect })
}

pub struct CandidateLinks {
    pub candidate_buttons: Vec<CandidateButton>,
    pub other_internal_links: HashSet<Url>,
}

fn candidate_links_from_html(body: &str, res_url: &Url) -> CandidateLinks {
    let doc = scraper::Html::parse_document(body);

    let img_selector = scraper::Selector::parse("img").unwrap();
    let anchor_selector = scraper::Selector::parse("a").unwrap();

    // this is used for deduplicating (so we don't have to linear search every time
    // to check)
    let mut existing_img_urls = HashSet::new();

    // get all of the img elements first, we're going to preserve the order of this
    // vec so the buttons are in the db in order (we can't just check for a >
    // img because some buttons don't have a tags)
    let mut candidate_img_els = Vec::new();
    for el in doc.select(&img_selector) {
        let src = el.value().attr("src").unwrap_or("");
        let Ok(src) = res_url.join(src) else {
            // not a valid src, skip
            continue;
        };
        let alt = el
            .value()
            .attr("alt")
            .map(|s| s.to_compact_string())
            .filter(|s| !s.is_empty());
        let title = el
            .value()
            .attr("title")
            .map(|s| s.to_compact_string())
            .filter(|s| !s.is_empty());

        // validate the width and height if present
        let width = el.value().attr("width").and_then(|s| s.parse::<u32>().ok());
        let height = el
            .value()
            .attr("height")
            .and_then(|s| s.parse::<u32>().ok());
        if !matches!((width, height), (None | Some(88), None | Some(31))) {
            continue;
        }

        // if the scheme isn't http, https, or data, skip
        // (data is for base64-encoded images, we like those)
        if !matches!(src.scheme(), "http" | "https" | "data") {
            continue;
        }
        // if the url path ends in .svg or .ico then skip because we don't support svg
        // or ico, sorry :(
        if src.path().ends_with(".svg") || src.path().ends_with(".ico") {
            // ok technically a website could have the path end in .svg and then serve a png
            // or something instead but idt any websites have buttons like that
            continue;
        }

        // deduplicate if the same img is present multiple times
        if existing_img_urls.contains(&src) {
            continue;
        }
        existing_img_urls.insert(src.clone());

        candidate_img_els.push(ButtonImgElement {
            url: src,
            alt,
            title,
        });
    }

    // now find the links for those img elements
    let mut candidate_buttons = Vec::new();
    let mut other_internal_links = HashSet::new();
    // add defaults for every button so they stay in order of the imgs
    for img in candidate_img_els.iter() {
        candidate_buttons.push(CandidateButton {
            img: img.clone(),
            href: None,
        });
    }

    // now check every anchor on the page to see if it matches one of the buttons
    for el in doc.select(&anchor_selector) {
        let href = el.value().attr("href").unwrap_or("");
        // validate the href before setting it
        if !matches!(href.chars().next(), Some('/' | '.' | 'a'..='z' | 'A'..='Z')) {
            // href must start with /, ., or a letter
            continue;
        }
        let Ok(href) = res_url.join(href) else {
            warn!("invalid href: {href}");
            continue;
        };
        if !matches!(href.scheme(), "http" | "https") {
            continue;
        }
        // remove any tracking params before saving it
        let href = transform_page_url_to_clean_up(href);

        if href.host_str() == res_url.host_str() && href != *res_url {
            // this is an internal link
            other_internal_links.insert(href.clone());
        }

        let Some(img_el) = el.select(&img_selector).next() else {
            // no img in this anchor, skip
            continue;
        };

        // get the src from the image since we're going to compare it
        let img_src = img_el.value().attr("src").unwrap_or("");
        let Ok(img_src) = res_url.join(img_src) else {
            continue;
        };

        // check that that image is one of our known buttons
        if !existing_img_urls.contains(&img_src) {
            continue;
        }

        let Some(button) = candidate_buttons.iter_mut().find(|b| b.img.url == img_src) else {
            // this shouldn't happen because we only add to existing_img_urls when we're
            // also adding to candidate_buttons
            unreachable!();
        };

        // if the href is the same as the image source, then it's not a page link and it
        // should be skipped
        if href == img_src {
            continue;
        }

        button.href = Some(href);
    }

    // only keep non-buttons in other_internal_links
    other_internal_links.retain(|link| {
        !candidate_buttons
            .iter()
            .any(|button| button.href.as_ref() == Some(link))
    });

    CandidateLinks {
        candidate_buttons,
        other_internal_links,
    }
}

/// Remove tracking parameters and unnecessary things from page urls.
fn transform_page_url_to_clean_up(mut url: Url) -> Url {
    let query_pairs = url.query_pairs().into_owned();
    let mut new_query_pairs = Vec::new();
    for (key, value) in query_pairs {
        if !crate::KNOWN_TRACKING_PARAMS.contains(&key.as_str()) {
            new_query_pairs.push((key, value));
        }
    }
    if new_query_pairs.is_empty() {
        url.set_query(None);
    } else {
        url.set_query(Some(
            &url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(new_query_pairs)
                .finish(),
        ));
    }

    // if it's www.youtube.com/watch then remove any params except v
    if url.host_str() == Some("www.youtube.com") && url.path() == "/watch" {
        if let Some((_, video_id)) = url.query_pairs().find(|(key, _)| key == "v") {
            url.set_query(Some(&format!("v={video_id}")));
        }
    }

    // convert youtu.be/x to www.youtube.com/watch?v=x
    if url.host_str() == Some("youtu.be") {
        if let Some(video_id) = url.path_segments().and_then(|mut s| s.next()) {
            url = match Url::parse_with_params("https://www.youtube.com/watch", &[("v", video_id)])
            {
                Ok(url) => url,
                Err(_) => url,
            };
        }
    }

    // not really a tracking param but remove url fragments (the #)
    url.set_fragment(None);

    // remove port if it's 80 or 443
    if matches!(url.port(), Some(80) | Some(443)) {
        let _ = url.set_port(None);
    }

    url
}

#[derive(Clone, Debug)]
pub struct ButtonImgElement {
    pub url: Url,
    pub alt: Option<CompactString>,
    pub title: Option<CompactString>,
}

#[derive(Clone, Debug)]
pub struct CandidateButton {
    pub img: ButtonImgElement,
    pub href: Option<Url>,
}

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
    str::FromStr,
    sync::Arc,
};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_with::{DeserializeFromStr, SerializeDisplay};
use tracing::{error, trace};
use url::Url;

use crate::{check_hosts_list_contains_url, BANNED_HOSTS, RECRAWL_PAGES_INTERVAL_HOURS};

/// Something that uniquely identifies a page. You can convert a URL to this,
/// but you can't convert it back to a URL since PageId removes information.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, SerializeDisplay, DeserializeFromStr, PartialOrd, Ord,
)]
pub struct PageId {
    pub host: String,
    /// The path of the page, without the leading slash.
    pub path: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CrawlData {
    crawling_pages: Vec<Url>,
    queue: VecDeque<Url>,
    pages: HashMap<PageId, Page>,

    // this is a hashset so it can be looked up quickly
    #[serde(default)]
    #[serde(skip)]
    pub queued_or_crawling_pages: HashSet<PageId>,
    #[serde(default)]
    #[serde(skip)]
    /// All the page ids that we've either crawled, are crawling, or have been
    /// redirected from before. We use this to avoid unnecessarily requeue
    pub known_page_ids: HashSet<PageId>,
    /// A cache of urls to button hashes. This is only used when a button is
    /// failed to be requested.
    #[serde(default)]
    #[serde(skip)]
    pub button_cache: Arc<RwLock<HashMap<Url, CachedButton>>>,
}

impl PartialEq for CrawlData {
    fn eq(&self, other: &Self) -> bool {
        self.crawling_pages == other.crawling_pages
            && self.queue == other.queue
            && self.pages == other.pages
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CachedButton {
    pub hash: String,
    pub file_ext: String,
    pub last_visited: chrono::DateTime<chrono::Utc>,
}

pub async fn save_crawl_data(crawl_data: &CrawlData) {
    // save to data/crawl.json (but a temp directory first, and then rename)
    let data_json = serde_json::to_string_pretty(crawl_data).unwrap();
    tokio::fs::write("data/crawl.json.bak", data_json)
        .await
        .unwrap();
    tokio::fs::rename("data/crawl.json.bak", "data/crawl.json")
        .await
        .unwrap();
}

pub async fn load_crawl_data() -> CrawlData {
    let mut crawl_data: CrawlData =
        if let Ok(data_json) = tokio::fs::read_to_string("data/crawl.json").await {
            serde_json::from_str(&data_json).unwrap()
        } else {
            CrawlData::default()
        };
    crawl_data.init();
    crawl_data.refresh_queue();
    crawl_data
}

impl CrawlData {
    fn init(&mut self) {
        for url in self.queue.iter() {
            if !check_hosts_list_contains_url(BANNED_HOSTS, url) {
                self.queued_or_crawling_pages
                    .insert(PageId::from(url.clone()));
            }
        }
        for url in self.crawling_pages.iter() {
            if !check_hosts_list_contains_url(BANNED_HOSTS, url) {
                self.queued_or_crawling_pages
                    .insert(PageId::from(url.clone()));
            }
        }

        self.known_page_ids
            .extend(self.queued_or_crawling_pages.clone());

        // create button cache
        let mut button_cache = self.button_cache.write();
        for (page_id, page) in &self.pages {
            self.known_page_ids.insert(page_id.clone());
            for redirect in &page.redirects {
                self.known_page_ids.insert(redirect.from.clone());
            }
            for button in &page.buttons {
                if let Some(source) = &button.source {
                    button_cache.insert(
                        source.clone(),
                        CachedButton {
                            hash: button.hash.clone(),
                            file_ext: button.file_ext.clone(),
                            last_visited: button.last_visited,
                        },
                    );
                }
            }
        }
        drop(button_cache);

        // add any pages in crawling_pages to the front of the queue
        if !self.crawling_pages.is_empty() {
            let mut new_queue = VecDeque::new();
            for url in self.crawling_pages.drain(..) {
                new_queue.push_back(url);
            }
            new_queue.extend(self.queue.drain(..));
            self.queued_or_crawling_pages.clear();

            for item in new_queue.iter() {
                self.add_to_queue(item.clone());
            }
        }

        if self.queue.is_empty() && self.pages.is_empty() {
            // default page if there's no data
            self.add_to_queue(Url::parse("https://matdoes.dev/retro").unwrap());
        }
    }

    /// Remove the URL from queue and add it to crawling_pages.
    pub fn start_crawling(&mut self, url: Url) {
        let page_id = PageId::from(url.clone());
        self.queue.retain(|u| u != &url);
        self.crawling_pages.push(url);
        self.queued_or_crawling_pages.insert(page_id.clone());
        self.known_page_ids.insert(page_id);
    }

    /// Remove the URL from crawling_pages and queued_or_crawling_pages.
    pub fn finish_crawling(&mut self, url: Url) {
        let page_id = PageId::from(url.clone());
        self.crawling_pages.retain(|u| u != &url);
        self.queued_or_crawling_pages.remove(&page_id);
    }

    /// Add the URL to queue and queued_or_crawling_pages.
    pub fn add_to_queue(&mut self, url: Url) {
        if check_hosts_list_contains_url(BANNED_HOSTS, &url) {
            return;
        }

        let page_id = PageId::from(url.clone());
        if self.queued_or_crawling_pages.contains(&page_id) {
            return;
        }
        trace!("adding {url} to queue");
        self.queue.push_back(url);
        self.queued_or_crawling_pages.insert(page_id.clone());
        self.known_page_ids.insert(page_id);
    }

    pub fn refresh_queue(&mut self) {
        // add pages that haven't been requested in a week
        // OR if a page failed to load then wait an hour * 2^(failed times - 1)

        let mut adding_to_queue = Vec::new();
        for page in self.pages.values() {
            if page.failed > 0 {
                let wait_time =
                    std::time::Duration::from_secs(60 * 60 * 2u64.pow(page.failed as u32 - 1));
                if page.last_visited + wait_time < chrono::Utc::now() {
                    adding_to_queue.push(page.url.clone());
                }
            } else if page.last_visited
                + chrono::Duration::hours(RECRAWL_PAGES_INTERVAL_HOURS as i64)
                < chrono::Utc::now()
            {
                adding_to_queue.push(page.url.clone());
            }
        }

        // remove urls that are already in the queue or being crawled
        adding_to_queue.retain(|url| {
            let page_id = PageId::from(url.clone());
            !self.queued_or_crawling_pages.contains(&page_id)
        });

        for url in adding_to_queue {
            self.add_to_queue(url);
        }
    }

    pub fn get_page_mut(&mut self, page_id: &PageId) -> Option<&mut Page> {
        self.pages.get_mut(page_id)
    }

    pub fn insert_page(&mut self, page: Page) {
        let page_id = PageId::from(page.url.clone());
        for redirect in &page.redirects {
            self.known_page_ids.insert(redirect.from.clone());
        }

        // if a redirect is in pages then it has to be removed from there
        for new_redirect in &page.redirects {
            self.pages.remove(&new_redirect.from);
        }

        // add buttons to cache
        let mut button_cache = self.button_cache.write();
        for button in &page.buttons {
            if let Some(source) = &button.source {
                button_cache.insert(
                    source.clone(),
                    CachedButton {
                        hash: button.hash.clone(),
                        file_ext: button.file_ext.clone(),
                        last_visited: button.last_visited,
                    },
                );
            }
        }
        drop(button_cache);

        if let Some(existing_page) = self.pages.get_mut(&page_id) {
            existing_page.buttons = page.buttons;
            existing_page.last_visited = page.last_visited;
            existing_page.failed = page.failed;

            // extend redirects
            for new_redirect in page.redirects {
                if let Some(existing_redirect) = existing_page
                    .redirects
                    .iter_mut()
                    .find(|r| r.from == new_redirect.from)
                {
                    existing_redirect.last_visited = new_redirect.last_visited;
                } else {
                    self.known_page_ids.insert(new_redirect.from.clone());
                    existing_page.redirects.push(new_redirect);
                }
            }
        } else {
            self.pages.insert(page_id.clone(), page);
            self.known_page_ids.insert(page_id);
        }
    }

    pub fn queue(&self) -> &VecDeque<Url> {
        &self.queue
    }

    pub fn pages(&self) -> &HashMap<PageId, Page> {
        &self.pages
    }

    pub fn pop_and_start_crawling(&mut self) -> Option<Url> {
        let url = self.queue.pop_front()?;
        self.start_crawling(url.clone());
        Some(url)
    }

    pub fn is_page_id_known(&self, page_id: &PageId) -> bool {
        self.known_page_ids.contains(page_id)
    }

    pub fn is_in_queue_or_crawling(&self, page_id: &PageId) -> bool {
        self.queued_or_crawling_pages.contains(page_id)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Page {
    /// The canonical URL for viewing the page. This is pretty much a superset
    /// of the PageId.
    pub url: Url,
    pub last_visited: chrono::DateTime<chrono::Utc>,
    /// The number of times in a row we've failed to request this page. 0 if the
    /// last time was successful.
    #[serde(default)]
    #[serde(skip_serializing_if = "is_zero")]
    pub failed: usize,
    pub buttons: Vec<ButtonData>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub redirects: Vec<RedirectSource>,
}

// https://stackoverflow.com/a/53900684
/// This is only used for serialize
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(num: &usize) -> bool {
    *num == 0
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RedirectSource {
    pub from: PageId,
    pub last_visited: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ButtonData {
    /// The URL where the button is located. None if the image was linked to as
    /// a `data:` URI.
    pub source: Option<Url>,
    pub hash: String,
    pub file_ext: String,
    pub target: Option<Url>,
    pub last_visited: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect: Option<RedirectSource>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alt: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl fmt::Display for PageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // displayed with slash between host and path unless there's no path
        let path_with_leading_slash_if_necessary = if self.path.is_empty() {
            self.path.clone()
        } else {
            format!("/{}", self.path)
        };
        write!(f, "{}{path_with_leading_slash_if_necessary}", self.host)
    }
}

impl FromStr for PageId {
    type Err = url::ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (host, path) = s.split_once('/').unwrap_or((s, ""));
        Ok(Self {
            host: host.to_string(),
            path: path.to_string(),
        })
    }
}

impl From<Url> for PageId {
    fn from(url: Url) -> Self {
        let host = url.host_str().unwrap_or_else(|| {
            error!("url {url} has no host, this should be impossible");
            ""
        });
        let host = host.trim_start_matches("www.");
        let host = host.to_string();

        let path = url.path().to_string();
        let path = path.trim_start_matches('/');
        let path = path.trim_end_matches("/index.html");
        let path = path.to_string();

        Self { host, path }
    }
}

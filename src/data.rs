use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
    io::Write,
    str::FromStr,
    sync::Arc,
};

use compact_str::{format_compact, CompactString, ToCompactString};
use indexmap::IndexSet;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_with::{DeserializeFromStr, SerializeDisplay};
use tracing::{error, info, trace, warn};
use url::Url;

use crate::{
    check_hosts_list_contains_host, check_hosts_list_contains_url, pagerank::PageRank,
    ratelimiter::Ratelimiter, BANNED_HOSTS, DO_NOT_FOLLOW_LINKS_FROM_HOSTS,
    RECRAWL_PAGES_INTERVAL_HOURS, STARTING_POINT,
};

/// Something that uniquely identifies a page. You can convert a URL to this,
/// but you can't convert it back to a URL since PageId removes information.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, SerializeDisplay, DeserializeFromStr, PartialOrd, Ord,
)]
pub struct PageId {
    pub host: CompactString,
    /// The path of the page, without the leading slash.
    pub path: CompactString,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CrawlerState {
    /// Pages that will be requested next. This is created based on PageRank
    /// scores.
    #[serde(default)]
    #[serde(skip)]
    pub request_queue: VecDeque<Url>,

    pub pages: HashMap<PageId, Page>,

    #[serde(default)]
    #[serde(skip)]
    pub ratelimiter: Ratelimiter,

    /// Pages that are currently either in the queue or being requested.
    // this is a hashset so it can be looked up quickly
    #[serde(default)]
    #[serde(skip)]
    pub queued_or_crawling_pages: HashSet<PageId>,

    /// All the page ids that we've either crawled, are crawling, or have been
    /// redirected from before.
    #[serde(default)]
    #[serde(skip)]
    pub known_page_ids: IndexSet<PageId>,
    /// This is used when we're updating the queue based on pagerank, we should
    /// insert into here whenever we add a page to the pagerank struct. Keys are
    /// indexes of known_page_ids.
    #[serde(default)]
    #[serde(skip)]
    pub urls_for_unvisited_pages: HashMap<u32, Url>,

    /// A cache of urls to button hashes. This is only used when a button is
    /// failed to be requested.
    #[serde(default)]
    #[serde(skip)]
    pub button_cache: Arc<RwLock<HashMap<Url, CachedButton>>>,

    #[serde(default)]
    #[serde(skip)]
    pub pagerank: Arc<RwLock<PageRank>>,
}

impl PartialEq for CrawlerState {
    fn eq(&self, other: &Self) -> bool {
        self.pages == other.pages
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CachedButton {
    pub hash: String,
    pub file_ext: String,
    pub last_visited: chrono::DateTime<chrono::Utc>,
}

pub async fn save_crawl_data(crawl_data: &CrawlerState) {
    // save to data/crawl.json (but a temp directory first, and then rename)
    let data_json = serde_json::to_string_pretty(crawl_data).unwrap();
    tokio::fs::write("data/crawl.json.bak", data_json)
        .await
        .unwrap();
    tokio::fs::rename("data/crawl.json.bak", "data/crawl.json")
        .await
        .unwrap();
}

pub async fn load_crawl_data() -> CrawlerState {
    let mut crawl_data: CrawlerState =
        if let Ok(data_json) = tokio::fs::read_to_string("data/crawl.json").await {
            serde_json::from_str(&data_json).unwrap()
        } else {
            CrawlerState::default()
        };
    crawl_data.init();
    crawl_data.refresh_queue();
    crawl_data
}

impl CrawlerState {
    fn init(&mut self) {
        // remove banned hosts
        self.pages = self
            .pages
            .drain()
            .filter(|(page_id, _page)| !check_hosts_list_contains_host(BANNED_HOSTS, &page_id.host))
            .collect();

        self.known_page_ids
            .extend(self.queued_or_crawling_pages.clone());

        // create button cache & pagerank links
        let mut button_cache = self.button_cache.write();
        let mut pagerank_links = Vec::new();
        for (page_id, page) in &self.pages {
            self.known_page_ids.insert(page_id.clone());

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

            let (page_idx, _) = self.known_page_ids.insert_full(page_id.clone());
            let links = get_pagerank_links_from_one_page(
                page,
                &mut self.known_page_ids,
                &self.pages,
                &mut self.urls_for_unvisited_pages,
            );

            // now actually save the links
            if pagerank_links.len() <= page_idx {
                pagerank_links.resize(page_idx + 1, Vec::new());
            }
            pagerank_links[page_idx].extend(links.into_iter());
        }
        drop(button_cache);
        // now save the pagerank links data!
        {
            let mut pagerank = PageRank::with_capacity(self.known_page_ids.len());
            pagerank.add_links(&pagerank_links);
            for i in 1..=50 {
                info!("Doing initial PageRank iteration #{i}");
                pagerank.do_iteration();
            }

            *self.pagerank.write() = pagerank;
        }

        if self.pages.is_empty() {
            // default page if there's no data
            self.add_to_queue(Url::parse(STARTING_POINT).unwrap());
        }
    }

    /// Remove the URL from queue and add it to crawling_pages.
    pub fn start_crawling(&mut self, url: &Url) {
        let page_id = PageId::from(url);
        self.request_queue.retain(|u| u != url);
        self.queued_or_crawling_pages.insert(page_id.clone());
        self.known_page_ids.insert(page_id);
    }

    /// Remove the URL from crawling_pages and queued_or_crawling_pages.
    pub fn finish_crawling(&mut self, url: &Url) {
        let page_id = PageId::from(url);
        self.queued_or_crawling_pages.remove(&page_id);
    }

    /// Add the URL to queue and queued_or_crawling_pages.
    pub fn add_to_queue(&mut self, url: Url) {
        if check_hosts_list_contains_url(BANNED_HOSTS, &url) {
            return;
        }

        let page_id = PageId::from(&url);
        if self.queued_or_crawling_pages.contains(&page_id) {
            return;
        }
        trace!("adding {url} to queue");
        self.request_queue.push_back(url);
        self.queued_or_crawling_pages.insert(page_id.clone());
        self.known_page_ids.insert(page_id);
    }

    pub fn refresh_queue(&mut self) {
        // add pages that haven't been requested in a week
        // OR if a page failed to load, then wait an hour * 2^(failed times - 1)

        let mut pagerank = self.pagerank.write();

        for _ in 0..5 {
            pagerank.do_iteration();
        }

        // let mut out = std::fs::File::create("target/pagerank.txt.tmp").unwrap();
        // pagerank.write_top_scores(&mut out, 100_000, &self.known_page_ids);
        // std::fs::rename("target/pagerank.txt.tmp", "target/pagerank.txt").unwrap();

        let top_scores = pagerank.get_all_sorted();

        let mut adding_to_queue = Vec::new();
        for (page_idx, pagerank_score) in top_scores {
            // note that 0.15 is typically the lowest (due to the pagerank damping factor)
            if pagerank_score < 0.151 {
                // pagerank score too low, not worth scraping :sleeping:
                break;
            }
            let Some(page_id) = self.known_page_ids.get_index(page_idx as usize) else {
                warn!("getting page idx {page_idx} in known_page_ids failed!");
                continue;
            };
            if self.queued_or_crawling_pages.contains(&page_id) {
                // page is already being crawled, no point in adding it to the queue again
                continue;
            }

            if let Some(page) = self.pages.get(page_id) {
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
            } else {
                // this means that we haven't visited the page before
                let Some(url) = self.urls_for_unvisited_pages.remove(&page_idx) else {
                    // can happen for non-pages like redirects, we don't wanna visit those
                    warn!("getting page_id {page_id} in urls_for_unvisited_pages failed!");
                    continue;
                };

                adding_to_queue.push(url.clone());
            }

            if adding_to_queue.len() + self.request_queue.len() > 10_000 {
                // this is enough pages for now
                break;
            }
        }

        drop(pagerank);

        info!("Adding {} new urls to queue", adding_to_queue.len());

        for url in adding_to_queue {
            self.add_to_queue(url);
        }

        // let mut f = std::fs::File::create("target/queue.txt.tmp").unwrap();
        // for u in self.queue() {
        //     writeln!(f, "{u}").unwrap();
        // }
        // std::fs::rename("target/queue.txt.tmp", "target/queue.txt").unwrap();
    }

    pub fn get_page_mut(&mut self, page_id: &PageId) -> Option<&mut Page> {
        self.pages.get_mut(page_id)
    }

    pub fn insert_page(&mut self, page: Page) {
        let page_id = PageId::from(&page.url);

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
            existing_page.other_internal_links = page.other_internal_links;
            existing_page.last_visited = page.last_visited;
            existing_page.failed = page.failed;
        } else {
            self.pages.insert(page_id.clone(), page);
            self.known_page_ids.insert(page_id);
        }
    }

    pub fn queue(&self) -> &VecDeque<Url> {
        &self.request_queue
    }

    pub fn pages(&self) -> &HashMap<PageId, Page> {
        &self.pages
    }
    pub fn pages_mut(&mut self) -> &mut HashMap<PageId, Page> {
        &mut self.pages
    }

    pub fn pop_and_start_crawling(&mut self) -> Option<Url> {
        let url = pop_from_given_queue(&mut self.request_queue, &mut self.ratelimiter)?;
        self.start_crawling(&url);
        Some(url)
    }

    pub fn is_page_id_known(&self, page_id: &PageId) -> bool {
        self.known_page_ids.contains(page_id)
    }

    pub fn is_in_queue_or_crawling(&self, page_id: &PageId) -> bool {
        self.queued_or_crawling_pages.contains(page_id)
    }
}

pub fn get_pagerank_links_from_one_page(
    page: &Page,
    known_page_ids: &mut IndexSet<PageId>,
    pages: &HashMap<PageId, Page>,
    urls_for_unvisited_pages: &mut HashMap<u32, Url>,
) -> Vec<(u32, f32)> {
    let mut links = Vec::new();

    let is_no_follow = check_hosts_list_contains_url(DO_NOT_FOLLOW_LINKS_FROM_HOSTS, &page.url);

    if is_no_follow {
        // disregard the links if we're not allowed to follow links from this page
        return vec![];
    }

    for other_internal_link in &page.other_internal_links {
        let other_internal_link_page_id = PageId::from(other_internal_link);
        let (internal_link_idx, _) =
            known_page_ids.insert_full(other_internal_link_page_id.clone());
        // non-88x31 links have a weight of 0.02, we still explore them just in case
        // they have more 88x31s on other pages
        links.push((internal_link_idx as u32, 0.02));
        if !pages.contains_key(&other_internal_link_page_id) {
            urls_for_unvisited_pages.insert(internal_link_idx as u32, other_internal_link.clone());
        }
    }
    for button in &page.buttons {
        if let Some(target) = &button.target {
            let target_page_id = PageId::from(target);
            let (target_page_idx, _) = known_page_ids.insert_full(target_page_id.clone());
            links.push((target_page_idx as u32, 1.));
            if !pages.contains_key(&target_page_id) {
                urls_for_unvisited_pages.insert(target_page_idx as u32, target.clone());
            }
        }
    }

    if let Some(target) = &page.redirects_to
        && matches!(target.scheme(), "http" | "https")
    {
        let target_page_id = PageId::from(target);
        let (target_page_idx, _) = known_page_ids.insert_full(target_page_id.clone());
        // most redirects are bad so we give them a low weight :(
        links.push((target_page_idx as u32, 0.1));
        if !pages.contains_key(&target_page_id) {
            urls_for_unvisited_pages.insert(target_page_idx as u32, target.clone());
        }
    }

    links
}

fn pop_from_given_queue(queue: &mut VecDeque<Url>, ratelimiter: &mut Ratelimiter) -> Option<Url> {
    if queue.is_empty() {
        return None;
    }

    let mut queue_index = None;
    for (i, url) in queue.iter().enumerate() {
        // get the first url that's not being ratelimited
        if ratelimiter.try_request(url.host_str().unwrap_or_default()) {
            queue_index = Some(i);
            break;
        }
    }

    let queue_index = queue_index?;

    let url = queue
        .remove(queue_index)
        .expect("this index should still exist since we just got it from iterating the queue");

    Some(url)
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
    #[serde(skip_serializing_if = "IndexSet::is_empty")]
    pub other_internal_links: IndexSet<Url>,

    /// This isn't actually a page and it's just a redirect. The fields with
    /// links should be empty in this case.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirects_to: Option<Url>,
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
    pub alt: Option<CompactString>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<CompactString>,
}

impl ButtonData {
    pub fn source_filename(&self) -> Option<CompactString> {
        if let Some(source) = &self.source {
            let path = source.path().trim_end_matches('/');
            let filename = path.split('/').last().unwrap_or_default();
            let filename = filename.split('.').next().unwrap_or_default();
            Some(filename.to_compact_string())
        } else {
            None
        }
    }
}

impl fmt::Display for PageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // displayed with slash between host and path unless there's no path
        let path_with_leading_slash_if_necessary = if self.path.is_empty() {
            self.path.clone()
        } else {
            format_compact!("/{}", self.path)
        };
        write!(f, "{}{path_with_leading_slash_if_necessary}", self.host)
    }
}

impl FromStr for PageId {
    type Err = url::ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (host, path) = s.trim_end_matches('/').split_once('/').unwrap_or((s, ""));
        Ok(Self {
            host: host.to_compact_string(),
            path: path.to_compact_string(),
        })
    }
}

impl From<&Url> for PageId {
    fn from(url: &Url) -> Self {
        let host = url.host_str().unwrap_or_else(|| {
            // this can happen with mailto links
            error!("url {url} has no host, this should be impossible");
            ""
        });
        let host = host.trim_start_matches("www.");
        let host = host.to_compact_string();

        let path = url.path().to_string();
        let path = path.trim_start_matches('/');
        let path = path.trim_end_matches("/index.html");
        let path = path.trim_end_matches('/');
        let path = path.to_compact_string();

        Self { host, path }
    }
}

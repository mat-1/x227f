pub mod data;
pub mod garbagecollect;
pub mod processed;
pub mod ratelimiter;
pub mod scrape;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use data::{CrawlData, PageId};
use parking_lot::Mutex;
use tracing::{debug, error, info, level_filters::LevelFilter};
use tracing_subscriber::{prelude::*, EnvFilter};
use url::Url;

use crate::data::Page;

pub const USER_AGENT: &str =
    "Mozilla/5.0 (88x31 crawler by mat@matdoes.dev +https://github.com/mat-1/x227f)";
/// How many pages we're crawling at once.
///
/// You can set this to higher numbers like 100 and it'll work fine, but right
/// now I have it set to a low number to avoid hitting Neocities and
/// archive.org's ratelimits.
pub const CONCURRENT_CRAWLER_COUNT: usize = 20;
/// How often we should recheck pages in the database.
pub const RECRAWL_PAGES_INTERVAL_HOURS: u64 = 24 * 7;
/// How long buttons should be cached for. We won't explicitly go out and
/// download them when this time expires, but we will download them again next
/// time there's a page with them.
///
/// Usually you can keep this the same as `RECRAWL_PAGES_INTERVAL_HOURS`.
pub const RECRAWL_BUTTONS_INTERVAL_HOURS: u64 = 24 * 7;
/// Url params that should be removed from page links before following and
/// saving them.
pub const KNOWN_TRACKING_PARAMS: &[&str] = &["ref"];
/// Pages that we can scrape but shouldn't follow links from. This will also
/// include all subdomains.
pub const DO_NOT_FOLLOW_LINKS_FROM_HOSTS: &[&str] = &["web.archive.org"];
/// Hosts that shouldn't be scraped or indexed. Adding a host to this will
/// retroactively remove it from the database.
pub const BANNED_HOSTS: &[&str] = &[];

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::fs::File::create("x227f.log").unwrap())
                .with_ansi(false)
                .with_filter(
                    EnvFilter::builder()
                        .with_default_directive("x227f=trace".parse().unwrap())
                        .from_env_lossy(),
                ),
        )
        .with(
            tracing_subscriber::fmt::layer().with_filter(
                EnvFilter::builder()
                    .with_default_directive(LevelFilter::INFO.into())
                    .from_env_lossy()
                    .add_directive("html5ever=error".parse().unwrap())
                    .add_directive("oxipng=error".parse().unwrap()),
            ),
        )
        .init();

    init_deadlock_detection();

    // make data/ and data/buttons/
    tokio::fs::create_dir_all("data/buttons").await.unwrap();

    let initial_crawl_data = data::load_crawl_data().await;

    info!("Initial queue size: {}", initial_crawl_data.queue().len());

    let crawl_data = Arc::new(Mutex::new(initial_crawl_data.clone()));

    let ctx = scrape::ScrapeContext::new();

    let mut tasks = Vec::new();
    for _ in 0..CONCURRENT_CRAWLER_COUNT {
        let ctx = ctx.clone();
        let crawl_data = crawl_data.clone();
        tasks.push(tokio::spawn(async move {
            crawl_task(ctx, crawl_data).await;
        }));
    }

    let mut previous_saved_data: CrawlData = initial_crawl_data.clone();

    // we don't save crawl.json immediately so if the user accidentally messed up if
    // they stop the program quickly enough it won't delete the database
    processed::save_processed_crawl_data(&previous_saved_data)
        .await
        .unwrap();

    let mut last_garbage_collected_at: Option<Instant> = None;

    loop {
        // save and refresh queue every 5 seconds
        tokio::time::sleep(Duration::from_secs(5)).await;

        {
            let crawl_data_clone = crawl_data.lock().clone();
            // don't bother saving if the data is the same as before
            if crawl_data_clone != previous_saved_data {
                info!("saving...");
                let save_start = Instant::now();
                data::save_crawl_data(&crawl_data_clone).await;
                let save_duration = save_start.elapsed();
                info!("save took {save_duration:?}");
                let save_start = Instant::now();
                processed::save_processed_crawl_data(&crawl_data_clone)
                    .await
                    .unwrap();
                let save_duration = save_start.elapsed();
                info!("save processed took {save_duration:?}");
                previous_saved_data = crawl_data_clone;
            }
        }
        info!("refreshing queue");
        let mut crawl_data = crawl_data.lock();
        crawl_data.refresh_queue();

        if let Some(last_garbage_collected_at) = &mut last_garbage_collected_at {
            if last_garbage_collected_at.elapsed() > Duration::from_secs(60 * 60) {
                *last_garbage_collected_at = Instant::now();
                garbagecollect::delete_unlinked_buttons(&crawl_data);
            }
        } else {
            last_garbage_collected_at = Some(Instant::now());
            garbagecollect::delete_unlinked_buttons(&previous_saved_data);
        }

        info!("Queue size: {}", crawl_data.queue().len());
    }
}

async fn crawl_task(ctx: scrape::ScrapeContext, crawl_data: Arc<Mutex<CrawlData>>) {
    loop {
        while let Some((url, is_normal_priority)) = {
            // this is necessary since otherwise crawl_data stays locked
            {
                let mut crawl_data = crawl_data.lock();
                if let Some(url) = crawl_data.pop_and_start_crawling().clone() {
                    Some((url, true))
                } else if let Some(url) = crawl_data.pop_and_start_crawling_low_priority().clone() {
                    Some((url, false))
                } else {
                    None
                }
            }
        } {
            let mut already_scraping_pages_clone =
                crawl_data.lock().queued_or_crawling_pages.clone();
            already_scraping_pages_clone.remove(&PageId::from(url.clone()));

            // this is the original url since we might get redirected to a different url
            let original_url = url.clone();
            let original_url_page_id = PageId::from(original_url.clone());

            let scrape_page_res = match scrape::page::download_page(&ctx, url).await {
                Ok(download_page_res) => {
                    let new_page_id = PageId::from(download_page_res.res.url().clone());
                    if new_page_id != original_url_page_id
                        && crawl_data.lock().is_in_queue_or_crawling(&new_page_id)
                    {
                        // the page we got redirected to is already being
                        // crawled, so don't scrape it

                        Ok(None)
                    } else {
                        let button_cache = crawl_data.lock().button_cache.clone();
                        match scrape::page::scrape_page_from_download(
                            &ctx,
                            download_page_res,
                            &button_cache,
                        )
                        .await
                        {
                            Ok(scrape_page_res) => Ok(Some(scrape_page_res)),
                            Err(e) => Err(e),
                        }
                    }
                }
                Err(e) => Err(e),
            };

            match scrape_page_res {
                Ok(Some(page)) => {
                    // if it's low priority then we'll only save it if it had buttons
                    if is_normal_priority || !page.buttons.is_empty() {
                        // add the page to crawl_data
                        let mut crawl_data = crawl_data.lock();
                        crawl_data.insert_page(page.clone());
                        let do_not_follow_links = check_hosts_list_contains_url(
                            DO_NOT_FOLLOW_LINKS_FROM_HOSTS,
                            &page.url,
                        );
                        if !do_not_follow_links {
                            for button in page.buttons {
                                // add button targets to queue if they're previously unseen pages
                                if let Some(target) = button.target {
                                    let target_page_id = PageId::from(target.clone());
                                    if !crawl_data.is_page_id_known(&target_page_id) {
                                        crawl_data.add_to_queue(target.clone());
                                    }
                                }
                            }
                        }
                        for url in page.other_internal_links {
                            let url_page_id = PageId::from(url.clone());
                            if !crawl_data.is_page_id_known(&url_page_id) {
                                crawl_data.add_to_low_priority_queue(url.clone());
                            }
                        }
                    } else {
                        debug!("not adding {original_url} to database since it's low priority and has no buttons");
                    }
                }
                Ok(None) => {
                    // none means that we got redirected to somewhere that's
                    // already being crawled, so nothing to do
                    debug!(
                        "not adding {original_url} to database since we got redirected to a page that's already being crawled"
                    );
                }
                Err(e) => {
                    error!("error scraping page {original_url}: {e}");
                    if is_normal_priority {
                        let mut crawl_data = crawl_data.lock();
                        if let Some(existing_page) = crawl_data.get_page_mut(&original_url_page_id)
                        {
                            existing_page.last_visited = chrono::Utc::now();
                            existing_page.failed += 1;
                        } else {
                            crawl_data.insert_page(Page {
                                url: original_url.clone(),
                                last_visited: chrono::Utc::now(),
                                failed: 1,
                                buttons: vec![],
                                other_internal_links: vec![],
                                redirects: vec![],
                            });
                        }
                    }
                }
            };

            // don't call finish_crawling until the end so if the page points to itself it
            // doesn't get added twice
            crawl_data.lock().finish_crawling(original_url);
        }

        // queue is empty, wait a second and check again
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

pub fn check_hosts_list_contains_url(hosts_list: &[&str], url: &Url) -> bool {
    let page_domain = url.domain().unwrap_or_default();
    hosts_list
        .iter()
        .any(|&domain| page_domain == domain || page_domain.ends_with(&format!(".{domain}")))
}

fn init_deadlock_detection() {
    use parking_lot::deadlock;
    use std::thread;
    // Create a background thread which checks for deadlocks every 10s
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(10));
        let deadlocks = deadlock::check_deadlock();
        if deadlocks.is_empty() {
            continue;
        }

        println!("{} deadlocks detected", deadlocks.len());
        for (i, threads) in deadlocks.iter().enumerate() {
            println!("Deadlock #{i}");
            for t in threads {
                println!("Thread Id {:#?}", t.thread_id());
                println!("{:#?}", t.backtrace());
            }
        }
    });
}

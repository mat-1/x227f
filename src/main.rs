#![feature(let_chains)]

pub mod data;
pub mod garbagecollect;
mod pagerank;
pub mod processed;
pub mod ratelimiter;
pub mod scrape;

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use compact_str::CompactString;
use data::{CrawlerState, PageId};
use parking_lot::Mutex;
use tracing::{debug, error, info, level_filters::LevelFilter};
use tracing_subscriber::{prelude::*, EnvFilter};
use url::Url;

use crate::data::{get_pagerank_links_from_one_page, Page};

pub const USER_AGENT: &str =
    "Mozilla/5.0 (88x31 crawler by mat@matdoes.dev +https://github.com/mat-1/x227f)";
/// The maximum number of pages that we can be crawling at the same time.
pub const CONCURRENT_CRAWLER_COUNT: usize = 100;
/// How often we should recheck pages in the database.
pub const RECRAWL_PAGES_INTERVAL_HOURS: u64 = 24;
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
pub const DO_NOT_FOLLOW_LINKS_FROM_HOSTS: &[&str] = &[
    "web.archive.org",   // duplicates content
    "crimsongale.com",   // crawler abuse
    "paddyk45.de",       // crawler abuse
    "phoenix-search.jp", // too many pages
    "ranking.prb.jp",    // too many pages
];
/// Hosts that shouldn't be scraped or indexed. Adding a host to this will
/// retroactively remove it from the database.
pub const BANNED_HOSTS: &[&str] = &[
    "prlog.ru",                  // too many pages
    "strawberryfoundations.xyz", // crawler abuse
    "paddyk45.duckdns.org",      // crawler abuse
    "dvd-rank.com",              // nsfw
    "adult-plus.com",            // nsfw
];

/// The page that's requested if the database is empty.
pub const STARTING_POINT: &str = "https://matdoes.dev/retro";

/// Re-encode every 88x31 and exit instead of actually crawling.
pub const FIX_IMAGES_MODE: bool = false;

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

    if FIX_IMAGES_MODE {
        do_fix_images_mode().await;
        return;
    }

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

    let mut previous_saved_data: CrawlerState = initial_crawl_data.clone();

    // we don't save crawl.json immediately so if the user accidentally messed up if
    // they stop the program quickly enough it won't delete the database
    processed::save_processed_crawl_data(&previous_saved_data)
        .await
        .unwrap();

    let mut last_garbage_collected_at: Option<Instant> = None;

    loop {
        // save and refresh queue periodically
        tokio::time::sleep(Duration::from_secs(15)).await;

        {
            debug!("maybe saving");
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

async fn crawl_task(ctx: scrape::ScrapeContext, crawl_state: Arc<Mutex<CrawlerState>>) {
    loop {
        while let Some(url) = {
            // this is necessary since otherwise crawl_data stays locked
            let mut crawl_data = crawl_state.lock();
            crawl_data.pop_and_start_crawling().clone()
        } {
            let mut already_scraping_pages_clone =
                crawl_state.lock().queued_or_crawling_pages.clone();
            already_scraping_pages_clone.remove(&PageId::from(&url));

            // this is the original url since we might get redirected to a different url
            let original_url = url.clone();
            let original_url_page_id = PageId::from(&original_url);

            let scrape_page_res = match scrape::page::download_page(&ctx, url).await {
                Ok(download_page_res) => {
                    let new_page_id = PageId::from(download_page_res.res.url());
                    if new_page_id != original_url_page_id
                        && crawl_state.lock().is_in_queue_or_crawling(&new_page_id)
                    {
                        // the page we got redirected to is already being
                        // crawled, so don't scrape it

                        Ok(None)
                    } else {
                        let button_cache = crawl_state.lock().button_cache.clone();
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
                    debug!(
                        "adding {original_url} to database, which has {} buttons",
                        page.buttons.len()
                    );
                    // add the page to crawl_data
                    let mut crawl_state = crawl_state.lock();
                    crawl_state.insert_page(page.clone());
                    let page_id = PageId::from(&page.url);
                    let links = {
                        let CrawlerState {
                            ref mut known_page_ids,
                            ref pages,
                            ref mut urls_for_unvisited_pages,
                            ..
                        } = &mut *crawl_state;
                        get_pagerank_links_from_one_page(
                            &page,
                            known_page_ids,
                            pages,
                            urls_for_unvisited_pages,
                        )
                    };
                    let (page_idx, _) = crawl_state.known_page_ids.insert_full(page_id.clone());
                    crawl_state
                        .pagerank
                        .write()
                        .set_new_links_for_page(page_idx as u32, &links);
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
                    let mut crawl_data = crawl_state.lock();
                    if let Some(existing_page) = crawl_data.get_page_mut(&original_url_page_id) {
                        existing_page.last_visited = chrono::Utc::now();
                        existing_page.failed += 1;
                    } else {
                        crawl_data.insert_page(Page {
                            url: original_url.clone(),
                            last_visited: chrono::Utc::now(),
                            failed: 1,
                            buttons: vec![],
                            other_internal_links: Default::default(),
                            redirects_to: None,
                        });
                    }
                }
            };

            // don't call finish_crawling until the end so if the page points to itself it
            // doesn't get added twice
            crawl_state.lock().finish_crawling(&original_url);
        }

        // queue is empty, wait a second and check again
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

pub fn check_hosts_list_contains_url(hosts_list: &[&str], url: &Url) -> bool {
    let page_domain = url.domain().unwrap_or_default();
    check_hosts_list_contains_host(hosts_list, page_domain)
}

pub fn check_hosts_list_contains_host(hosts_list: &[&str], page_domain: &str) -> bool {
    hosts_list
        .iter()
        .any(|&domain| page_domain == domain || page_domain.ends_with(&format!(".{domain}")))
}

fn init_deadlock_detection() {
    use std::thread;

    use parking_lot::deadlock;
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

async fn do_fix_images_mode() {
    let mut crawl_data = data::load_crawl_data().await;

    // println!("Doing garbage collecting first");
    // garbagecollect::delete_unlinked_buttons(&crawl_data);

    println!("Starting!");

    let mut button_hash_to_page_id: HashMap<String, Vec<PageId>> = HashMap::new();
    let mut button_file_exts: HashMap<String, CompactString> = HashMap::new();
    for (page_id, page) in crawl_data.pages().iter() {
        for button in &page.buttons {
            button_hash_to_page_id
                .entry(button.hash.clone())
                .or_default()
                .push(page_id.clone());
            button_file_exts.insert(button.hash.clone(), button.file_ext.clone().into());
        }
    }

    let ctx = scrape::ScrapeContext::new();

    let total = button_hash_to_page_id.len();

    async fn re_encode_button_task(
        button_file_exts: &Arc<HashMap<String, CompactString>>,
        ctx: &scrape::ScrapeContext,
        crawl_data_pages_mut: Arc<Mutex<HashMap<PageId, Page>>>,
        old_hash: String,
        pages: Box<[PageId]>,
        i: usize,
        total: usize,
        // TODO: this was disabled during development, should probably be properly implemented now
        _to_delete: Arc<Mutex<Vec<(String, CompactString)>>>,
    ) {
        let file_ext = button_file_exts.get(&old_hash).unwrap();

        println!("{}/{total} \t| {old_hash}.{file_ext}", i + 1);

        // read data/buttons/{hash}.{file_ext}
        let button_data = match tokio::fs::read(format!("data/buttons/{old_hash}.{file_ext}")).await
        {
            Ok(button_data) => button_data,
            Err(_) => {
                // println!("{old_hash}.{file_ext} is missing!!!");
                // return;

                println!("{old_hash}.{file_ext} is missing, downloading..");

                let first_image_url = crawl_data_pages_mut
                    .lock()
                    .get_mut(pages.first().unwrap())
                    .unwrap()
                    .buttons
                    .iter()
                    .find(|button| button.hash == old_hash)
                    .unwrap()
                    .source
                    .clone()
                    .unwrap();

                let scrape::image::DownloadImageResult { bytes, .. } =
                    match scrape::image::download_88x31_image(&ctx, first_image_url).await {
                        Ok(res) => res,
                        Err(e) => {
                            println!("couldn't download {old_hash}.{file_ext}: {e}");
                            return;
                        }
                    };
                let image_hash = scrape::image::hash_image(&bytes);
                if image_hash != old_hash {
                    println!(
                        "warning: {old_hash}.{file_ext} is missing and the downloaded image has a different hash ({image_hash}), continuing anyways"
                    );
                    // save it twice, with different hashes
                    tokio::fs::write(format!("data/buttons/{old_hash}.{file_ext}"), &bytes)
                        .await
                        .unwrap();
                }
                tokio::fs::write(format!("data/buttons/{image_hash}.{file_ext}"), &bytes)
                    .await
                    .unwrap();

                bytes
            }
        };

        let Some(format) = image::ImageFormat::from_extension(&file_ext) else {
            println!("{old_hash}.{file_ext} skipped because we couldn't determine the format");
            return;
        };

        let start_time = Instant::now();
        let new_image = match scrape::image::re_encode_image(button_data, format) {
            Ok(new_image) => new_image,
            Err(e) => {
                println!("couldn't re-encode {old_hash}.{file_ext}: {e}");
                return;
            }
        };
        let elapsed = start_time.elapsed();

        let new_hash = scrape::image::hash_image(&new_image);

        if new_hash == old_hash {
            println!(
                "{}/{total} \t| {old_hash}.{file_ext} is already optimized - took {elapsed:?}",
                i + 1
            );
            return;
        }

        // write data/buttons/{new_hash}.{new_file_ext}
        for attempt in 0..3 {
            match tokio::fs::write(format!("data/buttons/{new_hash}.{file_ext}"), &new_image).await
            {
                Ok(_) => break,
                Err(e) => {
                    println!("couldn't write {new_hash}.{file_ext} (attempt {attempt}): {e}");
                }
            }
        }
        let mut crawl_data_pages_mut_lock = crawl_data_pages_mut.lock();
        for page in pages {
            let page = crawl_data_pages_mut_lock.get_mut(&page).unwrap();
            for button in &mut page.buttons {
                if button.hash == old_hash {
                    button.hash = new_hash.clone();
                }
            }
        }
        drop(crawl_data_pages_mut_lock);

        println!(
            "{}/{total} \t| {old_hash}.{file_ext} re-encoded to {new_hash}.{file_ext} - took {elapsed:?}",
            i + 1
        );
    }

    let mut tasks = Vec::new();

    let to_delete = Arc::new(Mutex::new(Vec::new()));
    let crawl_data_pages_mut = Arc::new(Mutex::new(crawl_data.pages().clone()));
    let button_file_exts = Arc::new(button_file_exts);

    for (i, (old_hash, pages)) in button_hash_to_page_id.into_iter().enumerate() {
        let button_file_exts = button_file_exts.clone();
        let ctx = ctx.clone();
        let crawl_data_pages_mut = crawl_data_pages_mut.clone();
        let to_delete = to_delete.clone();
        let pages = pages.clone().into_boxed_slice();
        tasks.push(tokio::spawn(async move {
            re_encode_button_task(
                &button_file_exts,
                &ctx,
                crawl_data_pages_mut,
                old_hash,
                pages,
                i,
                total,
                to_delete,
            )
            .await;
        }));
        if tasks.len() >= 100 {
            // pop the first task that's done
            for (i, task) in tasks.iter_mut().enumerate() {
                if task.is_finished() {
                    if let Err(e) = task.await {
                        println!("task failed: {e}");
                    }
                    tasks.remove(i);
                    break;
                }
            }
            // if it's *still* too long, just await the first task
            if tasks.len() >= 100 {
                // wait for the first task to finish
                if let Err(e) = tasks.remove(0).await {
                    println!("task failed: {e}");
                }
            }
        }
    }
    for task in tasks {
        if let Err(e) = task.await {
            println!("task failed: {e}");
        }
    }
    *crawl_data.pages_mut() = crawl_data_pages_mut.lock().clone();

    println!("saving new hashes...");
    data::save_crawl_data(&crawl_data).await;
    processed::save_processed_crawl_data(&crawl_data)
        .await
        .unwrap();

    println!("deleting images...");
    for (old_hash, file_ext) in to_delete.lock().drain(..) {
        // delete the old one
        if let Err(e) = tokio::fs::remove_file(format!("data/buttons/{old_hash}.{file_ext}")).await
        {
            println!("couldn't delete {old_hash}.{file_ext}: {e}");
        }
    }

    println!("DONE");
}

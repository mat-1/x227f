//! Convert the crawl.json into a smaller 88x31.{json,cbor} that's easier to do
//! searches and calculations on.

use std::collections::{BTreeSet, HashMap};

use serde::Serialize;

use crate::data::{CrawlData, PageId};

#[derive(Serialize)]
pub struct ProcessedData {
    /// A sorted vec of page IDs
    pub pages: Vec<PageId>,
    /// A sorted vec of button hashes
    pub buttons: Vec<String>,
    /// A sorted vec of titles and alt texts used for buttons
    pub texts: Vec<String>,
    pub button_file_exts: Vec<String>,
    /// Indexes into `pages`
    pub links: Vec<Vec<usize>>,
    /// Indexes into `buttons`
    pub button_links: Vec<Vec<usize>>,
    pub button_link_alts: Vec<Vec<Option<usize>>>,
    pub button_link_titles: Vec<Vec<Option<usize>>>,

    pub backlinks: Vec<Vec<usize>>,
    pub button_backlinks: Vec<Vec<usize>>,
}

pub fn process_crawl_data(crawl_data: &CrawlData) -> ProcessedData {
    let mut pages = BTreeSet::new();
    for (page_id, page) in crawl_data.pages().iter() {
        pages.insert(page_id.clone());
        for link in &page.buttons {
            if let Some(target) = &link.target {
                let target_page_id = PageId::from(target.clone());
                pages.insert(target_page_id);
            }
        }
    }
    let pages = pages.into_iter().collect::<Vec<_>>();

    // fill buttons and button_file_exts
    let mut buttons = BTreeSet::new();
    let mut button_file_exts_hashmap = HashMap::new();
    for page in crawl_data.pages().values() {
        for button in &page.buttons {
            buttons.insert(button.hash.clone());
            button_file_exts_hashmap.insert(button.hash.clone(), button.file_ext.clone());
        }
    }
    let buttons = buttons.into_iter().collect::<Vec<_>>();

    // fill texts
    let mut texts = BTreeSet::new();
    for page in crawl_data.pages().values() {
        for button in &page.buttons {
            if let Some(title) = &button.title {
                texts.insert(title.clone());
            }
            if let Some(alt) = &button.alt {
                texts.insert(alt.clone());
            }
        }
    }
    let texts = texts.into_iter().collect::<Vec<_>>();

    let mut button_file_exts = Vec::new();
    for button in &buttons {
        button_file_exts.push(button_file_exts_hashmap[button].clone());
    }

    let mut links = vec![Vec::new(); pages.len()];
    let mut button_links = vec![Vec::new(); buttons.len()];
    let mut button_link_alts = vec![Vec::new(); buttons.len()];
    let mut button_link_titles = vec![Vec::new(); buttons.len()];

    let mut backlinks = vec![Vec::new(); pages.len()];
    let mut button_backlinks = vec![Vec::new(); buttons.len()];
    for (page_id, page) in crawl_data.pages().iter() {
        let page_id_index = pages
            .binary_search(page_id)
            .expect("page_id should be in pages");
        for link in &page.buttons {
            if let Some(target) = &link.target {
                let target_page_id = PageId::from(target.clone());
                let link_index = pages
                    .binary_search(&target_page_id)
                    .expect("link should be in pages");
                links[page_id_index].push(link_index);
                backlinks[link_index].push(page_id_index);
            }
        }
        for button in &page.buttons {
            let button_index = buttons
                .binary_search(&button.hash)
                .expect("button should be in buttons");
            button_links[button_index].push(page_id_index);
            let alt_index = button
                .alt
                .as_ref()
                .and_then(|alt| texts.binary_search(alt).ok());
            button_link_alts[button_index].push(alt_index);
            let title_index = button
                .title
                .as_ref()
                .and_then(|title| texts.binary_search(title).ok());
            button_link_titles[button_index].push(title_index);
            button_backlinks[page_id_index].push(button_index);
        }
    }

    ProcessedData {
        pages,
        buttons,
        texts,
        button_file_exts,

        links,
        button_links,
        button_link_alts,
        button_link_titles,

        backlinks,
        button_backlinks,
    }
}

pub async fn save_processed_crawl_data(crawl_data: &CrawlData) -> eyre::Result<()> {
    let processed_data = process_crawl_data(crawl_data);
    let json = serde_json::to_string(&processed_data)?;
    tokio::fs::write("88x31.json.bak", json).await?;
    tokio::fs::rename("88x31.json.bak", "88x31.json").await?;

    let cbor = serde_cbor::to_vec(&processed_data)?;
    tokio::fs::write("88x31.cbor.bak", cbor).await?;
    tokio::fs::rename("88x31.cbor.bak", "88x31.cbor").await?;

    Ok(())
}

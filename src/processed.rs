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

    /// Same length as `buttons`
    pub button_file_exts: Vec<String>,
    /// Same length as `buttons`, indexes into `texts`
    pub button_names: Vec<Vec<usize>>,
    /// Same length as `buttons`, indexes into `pages`
    pub button_backlinks: Vec<Vec<usize>>,

    /// Same length as `pages`, indexes into `pages`
    pub links: Vec<Vec<Option<usize>>>,
    /// Same length as `pages`, indexes into `buttons`
    pub link_buttons: Vec<Vec<usize>>,
    /// Same length as `pages`, indexes into `texts`
    pub link_button_alts: Vec<Vec<Option<usize>>>,
    /// Same length as `pages`, indexes into `texts`
    pub link_button_titles: Vec<Vec<Option<usize>>>,
    /// Same length as `pages`, indexes into `texts`
    pub link_button_filenames: Vec<Vec<Option<usize>>>,

    /// Same length as `pages`, indexes into `pages`
    pub backlinks: Vec<Vec<usize>>,
    /// Same length as `pages`, indexes into `buttons`
    pub backlink_buttons: Vec<Vec<usize>>,
}

pub fn process_crawl_data(crawl_data: &CrawlData) -> ProcessedData {
    let mut redirects = HashMap::new();

    let mut pages = BTreeSet::new();
    for (page_id, page) in crawl_data.pages().iter() {
        pages.insert(page_id.clone());
        for link in &page.buttons {
            if let Some(target) = &link.target {
                let target_page_id = PageId::from(target.clone());
                pages.insert(target_page_id);
            }
        }
        for redirect in &page.redirects {
            redirects.insert(redirect.from.clone(), page_id.clone());
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
            if let Some(source_filename) = button.source_filename() {
                texts.insert(source_filename);
            }
        }
    }
    let texts = texts.into_iter().collect::<Vec<_>>();

    let mut button_file_exts = Vec::new();
    for button in &buttons {
        button_file_exts.push(button_file_exts_hashmap[button].clone());
    }

    let mut button_names = vec![Vec::new(); buttons.len()];
    let mut button_backlinks = vec![Vec::new(); buttons.len()];

    let mut links = vec![Vec::new(); pages.len()];
    let mut link_buttons = vec![Vec::new(); pages.len()];
    let mut link_button_alts = vec![Vec::new(); pages.len()];
    let mut link_button_titles = vec![Vec::new(); pages.len()];
    let mut link_button_filenames = vec![Vec::new(); pages.len()];

    let mut backlinks = vec![Vec::new(); pages.len()];
    let mut backlink_buttons = vec![Vec::new(); pages.len()];
    for (page_id, page) in crawl_data.pages().iter() {
        // follow redirects
        let page_id = redirects.get(page_id).unwrap_or(page_id);

        let page_id_index = pages
            .binary_search(page_id)
            .expect("page_id should be in pages");

        for button in &page.buttons {
            let button_index = buttons
                .binary_search(&button.hash)
                .expect("button should be in buttons");

            if let Some(target) = &button.target {
                let target_page_id = PageId::from(target.clone());
                // follow redirects (this doesn't need to be a loop since they're already
                // follows them all the way through)
                let target_page_id = redirects.get(&target_page_id).unwrap_or(&target_page_id);

                let link_index = pages
                    .binary_search(&target_page_id)
                    .expect("link should be in pages");
                links[page_id_index].push(Some(link_index));
                backlinks[link_index].push(page_id_index);
                backlink_buttons[link_index].push(button_index);
            } else {
                links[page_id_index].push(None);
            }
            link_buttons[page_id_index].push(button_index);

            let alt_index = button
                .alt
                .as_ref()
                .and_then(|alt| texts.binary_search(alt).ok());
            link_button_alts[page_id_index].push(alt_index);
            let title_index = button
                .title
                .as_ref()
                .and_then(|title| texts.binary_search(title).ok());
            link_button_titles[page_id_index].push(title_index);
            let filename_index = button
                .source_filename()
                .and_then(|filename| texts.binary_search(&filename).ok());
            link_button_filenames[page_id_index].push(filename_index);

            if let Some(alt_index) = alt_index {
                button_names[button_index].push(alt_index);
            }
            if let Some(title_index) = title_index {
                button_names[button_index].push(title_index);
            }
            if let Some(filename_index) = filename_index {
                button_names[button_index].push(filename_index);
            }
            button_backlinks[button_index].push(page_id_index);
        }
    }

    ProcessedData {
        pages,
        buttons,
        texts,

        button_file_exts,
        button_names,
        button_backlinks,

        links,
        link_buttons,
        link_button_alts,
        link_button_titles,
        link_button_filenames,

        backlinks,
        backlink_buttons,
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

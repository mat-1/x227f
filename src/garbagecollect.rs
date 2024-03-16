use std::collections::HashSet;

use tracing::info;

use crate::data::CrawlData;

pub fn delete_unlinked_buttons(crawl_data: &CrawlData) {
    info!("Garbage collecting images...");

    let mut linked_button_filenames = HashSet::new();
    for page in crawl_data.pages().values() {
        for button in &page.buttons {
            let filename = format!("{}.{}", button.hash, button.file_ext);
            linked_button_filenames.insert(filename);
        }
    }

    let mut deleting_button_filenames = HashSet::new();

    // delete anything from data/buttons that isn't in linked_button_filenames
    let mut button_dir = std::fs::read_dir("data/buttons").unwrap();
    while let Some(Ok(entry)) = button_dir.next() {
        let filename = entry.file_name().into_string().unwrap();
        if !linked_button_filenames.contains(&filename) {
            info!("Unused button: {filename}");
            deleting_button_filenames.insert(filename);
        }
    }

    if deleting_button_filenames.is_empty() {
        info!("No unlinked buttons to delete.");
    } else {
        info!(
            "Deleting {} unlinked buttons...",
            deleting_button_filenames.len()
        );

        for filename in deleting_button_filenames {
            println!("{filename}");
            // std::fs::remove_file(format!("data/buttons/{filename}")).
            // unwrap();
        }
    }
}

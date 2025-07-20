use std::io;

use indexmap::IndexSet;
use soa_rs::{Soa, Soars};
use tracing::debug;

use crate::data::PageId;

#[derive(Default, Debug)]
pub struct PageRank {
    pub pages: Soa<Page>,
}
#[derive(Soars, Clone)]
#[soa_derive(Debug)]
pub struct Page {
    pub num_outbound_links: u32,
    pub inbound_links_and_weights: Vec<(u32, f32)>,
    pub score: f32,
}

const DAMPING_FACTOR: f32 = 0.85;

impl PageRank {
    pub fn with_capacity(capacity: usize) -> Self {
        let mut pages = Soa::<Page>::with_capacity(capacity);
        for _ in 0..capacity {
            pages.push(Page {
                num_outbound_links: 0,
                inbound_links_and_weights: Vec::new(),
                score: 1.,
            });
        }
        Self { pages }
    }

    pub fn add_links(&mut self, map: &[Vec<(u32, f32)>]) {
        if map.len() > self.pages.len() {
            let additional = map.len() - self.pages.len();
            self.pages.reserve(additional);
            for _ in 0..additional {
                self.pages.push(Page {
                    num_outbound_links: 0,
                    inbound_links_and_weights: Vec::new(),
                    score: 1.,
                });
            }
        }

        for (from_id, to_links) in map.iter().enumerate() {
            *self.pages.idx_mut(from_id).num_outbound_links += to_links.len() as u32;
            for &(link, link_weight) in to_links {
                if link == from_id as u32 {
                    // pagerank breaks if we don't ignore self links
                    continue;
                }

                self.pages
                    .idx_mut(link as usize)
                    .inbound_links_and_weights
                    .push((from_id as u32, link_weight));
            }
        }
    }

    pub fn set_new_links_for_page(&mut self, page_idx: u32, new_links: &[(u32, f32)]) {
        if page_idx as usize >= self.pages.len() {
            let additional = page_idx as usize - self.pages.len() + 1;
            self.pages.reserve(additional);
            for _ in 0..additional {
                self.pages.push(Page {
                    num_outbound_links: 0,
                    inbound_links_and_weights: Vec::new(),
                    score: 1.,
                });
            }
        }

        *self.pages.idx_mut(page_idx as usize).num_outbound_links += new_links.len() as u32;
        for &(link, link_weight) in new_links {
            if link == page_idx {
                // self link
                continue;
            }

            if link as usize >= self.pages.len() {
                let additional = link as usize - self.pages.len() + 1;
                self.pages.reserve(additional);
                for _ in 0..additional {
                    self.pages.push(Page {
                        num_outbound_links: 0,
                        inbound_links_and_weights: Vec::new(),
                        score: 1.,
                    });
                }
            }
            self.pages
                .idx_mut(link as usize)
                .inbound_links_and_weights
                .push((page_idx as u32, link_weight));
        }
    }

    pub fn do_iteration(&mut self) {
        let mut new_scores = vec![0.; self.pages.len()];

        for (to_id, to_page) in self.pages.iter().enumerate() {
            let mut sum = 0.;
            for (from_id, link_weight) in to_page.inbound_links_and_weights {
                let from_page = self.pages.idx(*from_id as usize);
                let pagerank_of_from_id = *from_page.score;
                let number_of_outbound_links = *from_page.num_outbound_links;

                sum += (pagerank_of_from_id / number_of_outbound_links as f32) * link_weight;
            }

            new_scores[to_id] = (1. - DAMPING_FACTOR) + DAMPING_FACTOR * sum;
        }

        self.pages.score_mut().copy_from_slice(&new_scores);
    }

    pub fn get_all_sorted(&self) -> Vec<(u32, f32)> {
        let scores = self.pages.score().iter().collect::<Vec<_>>();
        let mut scores_with_id = scores
            .iter()
            .enumerate()
            .map(|(idx, &&score)| (idx as u32, score))
            .collect::<Vec<_>>();

        debug!("Sorting...");
        scores_with_id.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        debug!("Done sorting");

        scores_with_id
    }

    pub fn write_top_scores(&self, f: &mut dyn io::Write, n: usize, mapping: &IndexSet<PageId>) {
        let scores_with_uuid = self.get_all_sorted();

        for (idx, score) in scores_with_uuid.into_iter().take(n) {
            // println!("{}: {}", mapping[*uuid].simple(), score);
            let inbound_links = self.pages.idx(idx as usize).inbound_links_and_weights.len();
            let name = &mapping[idx as usize];
            writeln!(f, "{name}: {score} ({inbound_links})").unwrap();
        }
    }
}

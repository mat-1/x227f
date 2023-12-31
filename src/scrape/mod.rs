use crate::USER_AGENT;

pub mod image;
pub mod page;

#[derive(Clone)]
pub struct ScrapeContext {
    http: reqwest::Client,
}

impl ScrapeContext {
    pub fn new() -> Self {
        Self {
            http: reqwest::ClientBuilder::new()
                .user_agent(USER_AGENT)
                // requests shouldn't take more than 30 seconds
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap(),
        }
    }
}

impl Default for ScrapeContext {
    fn default() -> Self {
        Self::new()
    }
}

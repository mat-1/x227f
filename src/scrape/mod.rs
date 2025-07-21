use reqwest::{header, redirect, Response};
use url::Url;

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
                // there's a few sites that have broken certificates that we still want to accept
                .danger_accept_invalid_certs(true)
                // we handle redirects ourselves
                .redirect(redirect::Policy::none())
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

pub fn get_redirect_target(res: &Response) -> Option<Url> {
    if !res.status().is_redirection() {
        return None;
    };
    res.headers()
        .get(header::LOCATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|l| res.url().join(l).ok())
}

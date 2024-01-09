use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

#[derive(Default, Clone, Debug)]
pub struct Ratelimiter {
    host_delay_until: HashMap<String, Instant>,
}

impl Ratelimiter {
    /// Checks whether we can make a request to the host, and if so, updates the
    /// ratelimiter.
    pub fn try_request(&mut self, host: &str) -> bool {
        let CrawlDelayResponse { delay, host } = crawl_delay_for_host(host);
        if delay.as_secs() == 0 {
            // this host has no delay, don't bother checking the ratelimiter
            return true;
        }

        let now = Instant::now();

        let delay_until = self.host_delay_until.entry(host).or_insert(now);
        if *delay_until > now {
            return false;
        }

        *delay_until = now + delay;
        true
    }
}

pub struct CrawlDelayResponse {
    pub delay: Duration,
    pub host: String,
}

pub fn crawl_delay_for_host(host: &str) -> CrawlDelayResponse {
    let delay_seconds = match host {
        "jcink.net" => 10,
        "web.archive.org" => 10,
        "neocities.org" => 10,
        _ => {
            if let Some(shortened_host) = shorten_host(host) {
                return crawl_delay_for_host(&shortened_host);
            }
            0
        }
    };
    CrawlDelayResponse {
        delay: Duration::from_secs(delay_seconds),
        host: host.to_string(),
    }
}

pub fn shorten_host(host: &str) -> Option<String> {
    let parts = host.split(".").collect::<Vec<&str>>();
    if parts.len() > 2 {
        return Some(parts[1..].join("."));
    }
    None
}

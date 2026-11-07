//! The redirect policy for every client that carries a credential.
//!
//! reqwest follows up to 10 redirects by default and, on a cross-host hop,
//! strips only `Authorization`, `Cookie` and `Proxy-Authorization`. Anything
//! else rides along: a configured `X-Api-Key: ${secret}` header, and on a 307
//! or 308 the request body too, which for a token endpoint holds the client
//! secret. An open redirect on an upstream would hand both to a third party.
//!
//! So a credential-carrying client follows a redirect only within the origin
//! it started at, plus the one benign exception of `http` to `https` on the
//! same host. Anything else is an error naming both ends, rather than a 3xx
//! the caller has to interpret.

use reqwest::redirect::{Attempt, Policy};
use reqwest::Url;

/// reqwest's own default hop limit, kept.
const MAX_HOPS: usize = 10;

pub fn same_origin() -> Policy {
    Policy::custom(decide)
}

fn decide(attempt: Attempt) -> reqwest::redirect::Action {
    let Some(start) = attempt.previous().first() else {
        return attempt.stop();
    };
    if attempt.previous().len() > MAX_HOPS {
        return attempt.error(format!("too many redirects (over {MAX_HOPS})"));
    }
    if allowed(start, attempt.url()) {
        return attempt.follow();
    }
    let msg = format!(
        "refused redirect from {} to {}: a redirect off the configured origin would carry credentials",
        origin(start),
        origin(attempt.url())
    );
    attempt.error(msg)
}

fn allowed(from: &Url, to: &Url) -> bool {
    if from.host_str() != to.host_str() {
        return false;
    }
    let same = from.scheme() == to.scheme() && from.port_or_known_default() == to.port_or_known_default();
    // http -> https on the same host, at default ports: an upgrade, not a move.
    let upgrade = from.scheme() == "http" && to.scheme() == "https" && from.port().is_none() && to.port().is_none();
    same || upgrade
}

/// `scheme://host:port`, never the path or query, which may hold a secret.
fn origin(u: &Url) -> String {
    match u.port_or_known_default() {
        Some(p) => format!("{}://{}:{p}", u.scheme(), u.host_str().unwrap_or("")),
        None => format!("{}://{}", u.scheme(), u.host_str().unwrap_or("")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testserver::{self, Reply};

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn only_the_starting_origin_or_an_https_upgrade_is_allowed() {
        assert!(allowed(&url("https://api.x.com/a"), &url("https://api.x.com/b?c=1")));
        assert!(allowed(&url("http://api.x.com/a"), &url("https://api.x.com/a")));
        assert!(!allowed(&url("https://api.x.com/a"), &url("https://evil.com/a")));
        assert!(!allowed(&url("https://api.x.com/a"), &url("https://x.com/a")), "a parent domain is another origin");
        assert!(!allowed(&url("https://api.x.com/a"), &url("http://api.x.com/a")), "no downgrade");
        assert!(!allowed(&url("http://127.0.0.1:8080/a"), &url("http://127.0.0.1:9090/a")), "another port");
        assert!(!allowed(&url("http://h:8080/a"), &url("https://h:8443/a")), "an upgrade only at default ports");
    }

    #[test]
    fn the_error_names_origins_not_paths() {
        assert_eq!(origin(&url("https://api.x.com/v1/k?token=s3cret")), "https://api.x.com:443");
    }

    #[tokio::test]
    async fn a_cross_origin_redirect_never_reaches_the_other_host() {
        let elsewhere = testserver::spawn(|_, _| Reply::text("got it")).await;
        let target = format!("{}steal", elsewhere.url());
        let origin_srv = testserver::spawn(move |_, _| Reply::status(307).with_header("location", &target)).await;
        crate::crypto::install_provider();
        let client = reqwest::Client::builder().redirect(same_origin()).build().unwrap();
        let err = client
            .post(origin_srv.url())
            .header("x-api-key", "secret-key")
            .body("client_secret=s")
            .send()
            .await
            .expect_err("a cross-origin hop must fail");
        assert!(format!("{err:?}").contains("refused redirect"), "{err:?}");
        assert_eq!(elsewhere.hits(), 0, "the other host must never see the key or the body");
    }

    #[tokio::test]
    async fn a_same_origin_redirect_is_still_followed() {
        let srv = testserver::spawn(|_, got| {
            if got.path == "/old" {
                Reply::status(301).with_header("location", "/new")
            } else {
                Reply::text("moved")
            }
        })
        .await;
        crate::crypto::install_provider();
        let client = reqwest::Client::builder().redirect(same_origin()).build().unwrap();
        let resp = client.get(format!("{}old", srv.url())).send().await.expect("followed");
        assert_eq!(resp.text().await.unwrap(), "moved");
        assert_eq!(srv.hits(), 2);
    }
}

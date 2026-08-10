//! Cloudflare Worker: registration intake for the Veryl registry.
//!
//! Stateless by design: it validates the submission (via [`registry_common`]) and
//! fires one `repository_dispatch`; the index repo's Action does the git work. The
//! only compute is JSON + the `fetch`, so it stays within the Free plan's 10ms CPU
//! budget (awaiting `fetch`, and the backoff between retries, does not count).

use registry_common::Submission;
use std::time::Duration;
use worker::*;

/// A dropped `repository_dispatch` has no recovery path — `reverify` only ever
/// sees a submission that already reached the index or an open PR.
const DISPATCH_ATTEMPTS: usize = 3;
/// Waited before each attempt after the first. The CLI gives the whole POST 10s,
/// so this has to leave room for the attempts themselves.
const DISPATCH_BACKOFF_MS: [u64; DISPATCH_ATTEMPTS - 1] = [250, 750];

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    // `/-/submit` is namespaced under `/-/` (never a valid owner) so it cannot
    // collide with a docs path when the gallery, docs, and this Worker share one host.
    let path = req.path();
    let path = path.strip_suffix('/').unwrap_or(&path);
    if req.method() != Method::Post || path != "/-/submit" {
        return Response::error("Not Found", 404);
    }

    let submission: Submission = match req.json().await {
        Ok(s) => s,
        Err(_) => return Response::error("invalid JSON body", 400),
    };

    if let Err(e) = submission.validate() {
        return Response::error(format!("rejected: {e}"), 422);
    }

    match dispatch(&env, &submission).await {
        Ok(()) => Response::from_json(&serde_json::json!({
            "status": "accepted",
            "repo": submission.repo,
        }))
        .map(|r| r.with_status(202)),
        Err(e) => Response::error(format!("dispatch failed: {e}"), 502),
    }
}

async fn dispatch(env: &Env, submission: &Submission) -> Result<()> {
    let token = env.secret("GITHUB_TOKEN")?.to_string();
    let index_repo = env.var("INDEX_REPO")?.to_string();

    let url = format!("https://api.github.com/repos/{index_repo}/dispatches");
    let body = serde_json::to_string(&serde_json::json!({
        "event_type": "registry-submit",
        "client_payload": submission,
    }))?;

    let mut last = String::new();
    for attempt in 0..DISPATCH_ATTEMPTS {
        if attempt > 0 {
            Delay::from(Duration::from_millis(DISPATCH_BACKOFF_MS[attempt - 1])).await;
        }
        match try_dispatch(&url, &token, &body).await {
            Attempt::Accepted => return Ok(()),
            Attempt::Transient(e) => last = e,
            Attempt::Fatal(e) => return Err(Error::RustError(e)),
        }
    }

    Err(Error::RustError(format!(
        "{DISPATCH_ATTEMPTS} attempts, last: {last}"
    )))
}

/// Outcome of one attempt. `Fatal` answers the author right away instead of
/// spending the remaining attempts on a rejection that cannot change.
enum Attempt {
    Accepted,
    Transient(String),
    Fatal(String),
}

async fn try_dispatch(url: &str, token: &str, body: &str) -> Attempt {
    let request = match dispatch_request(url, token, body) {
        Ok(r) => r,
        // Nothing here can start succeeding on a later attempt.
        Err(e) => return Attempt::Fatal(e.to_string()),
    };

    let mut resp = match Fetch::Request(request).send().await {
        Ok(r) => r,
        Err(e) => return Attempt::Transient(format!("fetch failed: {e}")),
    };

    let status = resp.status_code();
    if (200..300).contains(&status) {
        return Attempt::Accepted;
    }

    let msg = format!("github {status}: {}", resp.text().await.unwrap_or_default());
    if retryable(status) {
        Attempt::Transient(msg)
    } else {
        Attempt::Fatal(msg)
    }
}

/// An Actions incident surfaces as 5xx and throttling as 429; every other
/// rejection would come back the same.
fn retryable(status: u16) -> bool {
    status == 429 || status >= 500
}

fn dispatch_request(url: &str, token: &str, body: &str) -> Result<Request> {
    let headers = Headers::new();
    headers.set("Authorization", &format!("Bearer {token}"))?;
    headers.set("Accept", "application/vnd.github+json")?;
    headers.set("User-Agent", "veryl-registry-worker")?;
    headers.set("Content-Type", "application/json")?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(body.into()));

    Request::new_with_init(url, &init)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_only_transient_statuses() {
        assert!(retryable(429));
        assert!(retryable(500));
        assert!(retryable(502));
        assert!(retryable(503));
        assert!(!retryable(401));
        assert!(!retryable(403));
        assert!(!retryable(404));
        assert!(!retryable(422));
    }

    #[test]
    fn backoff_fits_the_cli_timeout() {
        let total: u64 = DISPATCH_BACKOFF_MS.iter().sum();
        assert!(total <= 2_000, "{total}ms of backoff is too much");
    }
}

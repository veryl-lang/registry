//! Cloudflare Worker: registration intake for the Veryl registry.
//!
//! Stateless by design: it validates the submission (via [`registry_common`]) and
//! fires one `repository_dispatch`; the index repo's Action does the git work. The
//! only compute is JSON + one `fetch`, so it stays within the Free plan's 10ms CPU
//! budget (awaiting `fetch` does not count).

use registry_common::Submission;
use worker::*;

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
    let payload = serde_json::json!({
        "event_type": "registry-submit",
        "client_payload": submission,
    });

    let mut headers = Headers::new();
    headers.set("Authorization", &format!("Bearer {token}"))?;
    headers.set("Accept", "application/vnd.github+json")?;
    headers.set("User-Agent", "veryl-registry-worker")?;
    headers.set("Content-Type", "application/json")?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(serde_json::to_string(&payload)?.into()));

    let request = Request::new_with_init(&url, &init)?;
    let mut resp = Fetch::Request(request).send().await?;

    if (200..300).contains(&resp.status_code()) {
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(Error::RustError(format!(
            "github {}: {body}",
            resp.status_code()
        )))
    }
}

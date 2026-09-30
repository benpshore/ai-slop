//! The endpoint table. The router matches against it, and a test checks the
//! `OpenAPI` document (`openapi.json`, served at `/v1/openapi.json`) lists
//! exactly these methods and paths.

/// One endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Health,
    Version,
    OpenApi,
    ListJobs,
    CreateJobs,
    GetJob,
    DeleteJob,
    CancelJob,
    Events,
    Output,
}

/// Method, path template (`{name}` matches one non-empty segment) and route.
pub const ROUTES: &[(&str, &str, Route)] = &[
    ("GET", "/v1/health", Route::Health),
    ("GET", "/v1/version", Route::Version),
    ("GET", "/v1/openapi.json", Route::OpenApi),
    ("GET", "/v1/jobs", Route::ListJobs),
    ("POST", "/v1/jobs", Route::CreateJobs),
    ("GET", "/v1/jobs/{id}", Route::GetJob),
    ("DELETE", "/v1/jobs/{id}", Route::DeleteJob),
    ("POST", "/v1/jobs/{id}/cancel", Route::CancelJob),
    ("GET", "/v1/jobs/{id}/events", Route::Events),
    ("GET", "/v1/jobs/{id}/output/{n}", Route::Output),
];

/// The `OpenAPI` 3.1 description of `ROUTES`, written by hand.
pub const OPENAPI: &str = include_str!("openapi.json");

/// What a method and path resolve to.
#[derive(Debug, PartialEq, Eq)]
pub enum Matched<'a> {
    /// The route and the values of its `{…}` segments, in order.
    Found(Route, [&'a str; 2]),
    /// The path exists with other methods (listed for `Allow`).
    WrongMethod(Vec<&'static str>),
    NotFound,
}

/// The values `template`'s `{…}` segments take in `path`, if it matches.
fn fit<'a>(template: &str, path: &'a str) -> Option<[&'a str; 2]> {
    let mut params = [""; 2];
    let mut count = 0;
    let mut want = template.split('/');
    let mut have = path.split('/');
    loop {
        match (want.next(), have.next()) {
            (None, None) => return Some(params),
            (Some(w), Some(h)) if w.starts_with('{') => {
                if h.is_empty() || count == params.len() {
                    return None;
                }
                params[count] = h;
                count += 1;
            }
            (Some(w), Some(h)) if w == h => {}
            _ => return None,
        }
    }
}

pub fn resolve<'a>(method: &str, path: &'a str) -> Matched<'a> {
    let mut allowed = Vec::new();
    for (m, template, route) in ROUTES {
        if let Some(params) = fit(template, path) {
            if *m == method {
                return Matched::Found(*route, params);
            }
            allowed.push(*m);
        }
    }
    if allowed.is_empty() {
        Matched::NotFound
    } else {
        Matched::WrongMethod(allowed)
    }
}

#[cfg(test)]
mod tests {
    use super::{Matched, Route, resolve};

    #[test]
    fn paths_resolve_exactly() {
        assert_eq!(
            resolve("GET", "/v1/jobs/ab-1/output/0"),
            Matched::Found(Route::Output, ["ab-1", "0"])
        );
        assert_eq!(
            resolve("POST", "/v1/jobs"),
            Matched::Found(Route::CreateJobs, ["", ""])
        );
        assert_eq!(
            resolve("PUT", "/v1/jobs/x"),
            Matched::WrongMethod(vec!["GET", "DELETE"])
        );
        for missing in [
            "/v1/jobs/",
            "/v1//cancel",
            "/v1/jobs/x/output/",
            "/v1/jobs/x/output/0/extra",
            "/v2/health",
            "/",
            "",
            "/v1/health/",
        ] {
            assert_eq!(resolve("GET", missing), Matched::NotFound, "{missing:?}");
        }
    }
}

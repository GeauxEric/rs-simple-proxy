use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use http::uri::{Parts, Uri};
use hyper::header::HeaderValue;
use hyper::{Body, Request, StatusCode};
use regex::Regex;
use serde_json;
use tokio::time::{self, Duration};

use crate::proxy::error::MiddlewareError;
use crate::proxy::middleware::MiddlewareResult::Next;
use crate::proxy::middleware::{Middleware, MiddlewareResult};
use crate::proxy::service::{ServiceContext, State};

// #[derive(Clone)]
pub struct Router {
    routes: RouterRules,
    name: String,
    limiters: RouteLimiters,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RouteRegex {
    #[serde(with = "serde_regex")]
    pub host: Regex,
    #[serde(with = "serde_regex")]
    pub path: Regex,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LimitReq {
    pub threshold_per_sec: usize,
    pub key: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Route {
    pub from: RouteRegex,
    pub to: RouteRegex,
    pub public: bool,
    pub limit_config: Option<LimitReq>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RouterRulesWrapper {
    pub rules: RouterRules,
}

pub type RouterRules = Vec<Route>;
type RouteLimiters = Vec<Option<Limiter>>;

#[derive(Serialize, Deserialize, Debug)]
pub struct MatchedRoute {
    pub uri: String,
    pub public: bool,
}

pub trait RouterConfig {
    fn get_router_filename(&self) -> &str;
}

fn get_host_and_path(req: &mut Request<Body>) -> Result<(String, String), MiddlewareError> {
    let uri = req.uri();
    let path = uri
        .path_and_query()
        .map(ToString::to_string)
        .unwrap_or_else(|| String::from(""));

    match uri.host() {
        Some(host) => Ok((String::from(host), path)),
        None => Ok((
            String::from(req.headers().get("host").unwrap().to_str()?),
            path,
        )),
    }
}

fn inject_new_uri(
    req: &mut Request<Body>,
    old_host: &str,
    host: &str,
    path: &str,
) -> Result<(), MiddlewareError> {
    {
        let headers = req.headers_mut();

        headers.insert("X-Forwarded-Host", HeaderValue::from_str(old_host).unwrap());
        headers.insert("host", HeaderValue::from_str(host).unwrap());
    }
    let mut parts = Parts::default();
    parts.scheme = Some("http".parse()?);
    parts.authority = Some(host.parse()?);
    parts.path_and_query = Some(path.parse()?);

    debug!("Found a route to {:?}", parts);

    *req.uri_mut() = Uri::from_parts(parts)?;

    Ok(())
}

impl Middleware for Router {
    fn name() -> String {
        String::from("Router")
    }

    fn before_request(
        &mut self,
        req: &mut Request<Body>,
        context: &ServiceContext,
        state: &State,
    ) -> Result<MiddlewareResult, MiddlewareError> {
        let routes = &self.routes;

        let (host, path) = get_host_and_path(req)?;
        debug!("Routing => Host: {} Path: {}", host, path);

        for (i, route) in routes.iter().enumerate() {
            let (re_host, re_path) = (&route.from.host, &route.from.path);
            let to = &route.to;
            let public = route.public;

            debug!("Trying to convert from {} / {:?}", &re_host, &re_path);

            if re_host.is_match(&host) {
                let new_host = re_host.replace(&host, to.host.as_str());

                let new_path = if re_path.is_match(&path) {
                    re_path.replace(&path, to.path.as_str())
                } else {
                    continue;
                };

                if let Some(limiter) = &self.limiters[i] {
                    // Ideally, the key is built based on configuration
                    // we just use the client's IP address as POC
                    let ip = context.remote_addr.ip().to_string();
                    if !limiter.accept(&ip) {
                        return Err(MiddlewareError::new(
                            "Too many requests".to_string(),
                            None,
                            StatusCode::from_u16(429).unwrap(),
                        ));
                    }
                }

                debug!("Proxying to {}", &new_host);
                inject_new_uri(req, &host, &new_host, &new_path)?;
                self.set_state(
                    context.req_id,
                    state,
                    serde_json::to_string(&MatchedRoute {
                        uri: req.uri().to_string(),
                        public,
                    })?,
                )?;
                return Ok(Next);
            }
        }

        Err(MiddlewareError::new(
            String::from("No route matched"),
            Some(String::from("Not found")),
            StatusCode::NOT_FOUND,
        ))
    }
}

fn read_routes<T: RouterConfig>(config: &T) -> RouterRules {
    use std::fs::File;
    use std::io::prelude::Read;

    let mut f = File::open(config.get_router_filename()).expect("Router config not found !");

    let mut data = String::new();
    f.read_to_string(&mut data)
        .expect("Cannot read Router config !");

    let rules: RouterRulesWrapper =
        serde_json::from_str(&data).expect("Cannot parse Router config file !");

    rules.rules
}

impl Router {
    pub fn new<T: RouterConfig>(config: &T) -> Self {
        let mut router = Router {
            routes: read_routes(config),
            name: String::from("Router"),
            limiters: vec![],
        };
        for route in &router.routes {
            if let Some(limit_config) = &route.limit_config {
                router.limiters.push(Some(Limiter::new(limit_config)));
            } else {
                router.limiters.push(None);
            }
        }

        router
    }
}

#[derive(Clone)]
struct Limiter {
    inner: Arc<Mutex<HashMap<String, u64>>>,
}

impl Limiter {
    pub fn new(limit_config: &LimitReq) -> Limiter {
        // let mut interval = time::interval(Duration::from_millis(50));
        let inner = Arc::new(Mutex::new(HashMap::new()));
        let limiter = Limiter { inner };

        let to_update = limiter.clone();
        let config = limit_config.clone();
        // TODO: it is unclear to me the life cycle of such tokio tasks
        let _clock = tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_millis(
                1000 / config.threshold_per_sec as u64,
            ));
            loop {
                interval.tick().await;
                to_update.leak();
            }
        });
        limiter
    }

    fn accept(&self, key: &str) -> bool {
        self.try_fill(key).is_ok()
    }

    /// Let each bucket leak one drop
    /// No-op if the bucket is already empty
    fn leak(&self) {
        let mut map = self.inner.lock().unwrap();
        for (_, val) in map.iter_mut() {
            if *val > 0 {
                *val -= 1;
            }
        }
    }

    /// Try to fill a bucket
    /// Return err if the bucket is full
    fn try_fill(&self, bucket_id: &str) -> Result<(), &str> {
        // TODO: read from config
        let buffer_size = 10;
        let mut map = self.inner.lock().unwrap();
        return if let Some(queue_size) = map.get_mut(bucket_id) {
            if *queue_size == buffer_size {
                Err("full")
            } else {
                *queue_size += 1;
                Ok(())
            }
        } else {
            map.insert(bucket_id.to_string(), 1);
            Ok(())
        };
    }
}

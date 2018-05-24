extern crate futures;
extern crate http;
extern crate hyper;

use futures::future;
use futures::future::IntoFuture;
use hyper::client::connect::HttpConnector;
use hyper::rt::Future;
use hyper::service::Service;
use hyper::{Body, Client, Request};
use std::fmt::Debug;
use std::sync::Arc;

use rand::prelude::*;
use rand::rngs::SmallRng;
use rand::FromEntropy;

use Middlewares;

type BoxFut = Box<Future<Item = hyper::Response<Body>, Error = hyper::Error> + Send>;

pub struct ProxyService {
  client: Client<HttpConnector, Body>,
  middlewares: Middlewares,
  rng: SmallRng,
}

fn convert_uri(uri: &hyper::Uri) -> hyper::Uri {
  let base: hyper::Uri = "http://localhost:4567".parse().unwrap();
  let mut parts: http::uri::Parts = base.into();
  if let Some(path_and_query) = uri.path_and_query() {
    parts.path_and_query = Some(path_and_query.clone());
  }

  hyper::Uri::from_parts(parts).unwrap() // Consider removing unwrap
}

fn convert_req<U: Debug>(base: hyper::Request<U>) -> hyper::Request<U> {
  let (mut parts, body) = base.into_parts();

  parts.uri = convert_uri(&parts.uri);

  hyper::Request::from_parts(parts, body)
}

impl Service for ProxyService {
  type Error = hyper::Error;
  type Future = BoxFut;
  type ReqBody = Body;
  type ResBody = Body;

  fn call(&mut self, req: Request<Self::ReqBody>) -> Self::Future {
    let mut req = convert_req(req);

    let mws_failure = Arc::clone(&self.middlewares);
    let mws_success = Arc::clone(&self.middlewares);

    // let req_id = rng.sample_iter(&Alphanumeric).take(7).collect();
    let req_id = self.rng.next_u64();

    for mw in self.middlewares.lock().unwrap().iter_mut() {
      if let Err(err) = mw.before_request(&mut req, req_id) {
        error!(
          "[{}] Error during request_failure callback: {:?}",
          mw.get_name(),
          err
        );
      }
    }

    let res = self
      .client
      .request(req)
      .map_err(move |err| {
        for mw in mws_failure.lock().unwrap().iter_mut() {
          if let Err(err) = mw.request_failure(&err, req_id) {
            error!(
              "[{}] Error during request_failure callback: {:?}",
              mw.get_name(),
              err
            );
          }
        }
        for mw in mws_failure.lock().unwrap().iter_mut() {
          if let Err(err) = mw.after_request(req_id) {
            error!(
              "[{}] Error during after_request callback: {:?}",
              mw.get_name(),
              err
            );
          }
        }
        err
      })
      .map(move |mut res| {
        for mw in mws_success.lock().unwrap().iter_mut() {
          if let Err(err) = mw.request_success(&mut res, req_id) {
            error!(
              "[{}] Error during request_success callback: {:?}",
              mw.get_name(),
              err
            );
          }
        }
        for mw in mws_success.lock().unwrap().iter_mut() {
          if let Err(err) = mw.after_request(req_id) {
            error!(
              "[{}] Error during after_success callback: {:?}",
              mw.get_name(),
              err
            );
          }
        }
        res
      });

    Box::new(res)
  }
}

impl ProxyService {
  pub fn new(middlewares: Middlewares) -> Self {
    ProxyService {
      client: Client::new(),
      rng: SmallRng::from_entropy(),
      middlewares,
    }
  }
}

impl IntoFuture for ProxyService {
  type Future = future::FutureResult<Self::Item, Self::Error>;
  type Item = Self;
  type Error = hyper::Error;

  fn into_future(self) -> Self::Future {
    future::ok(self)
  }
}

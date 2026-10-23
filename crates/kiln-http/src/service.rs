//! What serves requests: a request in, a response out, no error (a failure is a response).

use std::future::Future;

use bytes::Bytes;

use crate::Incoming;

/// A response body a connection can send.
pub trait SendBody: http_body::Body<Data = Bytes, Error: Into<Box<dyn std::error::Error + Send + Sync>>> {}

impl<B> SendBody for B where B: http_body::Body<Data = Bytes, Error: Into<Box<dyn std::error::Error + Send + Sync>>> {}

/// Serves the requests of a connection, one call per request.
pub trait Service {
    type Body: SendBody;
    type Future: Future<Output = http::Response<Self::Body>>;

    fn call(&self, req: http::Request<Incoming>) -> Self::Future;
}

/// A function as a [`Service`].
#[derive(Clone, Copy, Debug)]
pub struct ServiceFn<F>(F);

/// Serves requests with `f`.
pub fn service_fn<F, Fut, B>(f: F) -> ServiceFn<F>
where
    F: Fn(http::Request<Incoming>) -> Fut,
    Fut: Future<Output = http::Response<B>>,
    B: SendBody,
{
    ServiceFn(f)
}

impl<F, Fut, B> Service for ServiceFn<F>
where
    F: Fn(http::Request<Incoming>) -> Fut,
    Fut: Future<Output = http::Response<B>>,
    B: SendBody,
{
    type Body = B;
    type Future = Fut;

    fn call(&self, req: http::Request<Incoming>) -> Fut {
        (self.0)(req)
    }
}

impl<S: Service> Service for &S {
    type Body = S::Body;
    type Future = S::Future;

    fn call(&self, req: http::Request<Incoming>) -> S::Future {
        (**self).call(req)
    }
}

impl<S: Service> Service for std::sync::Arc<S> {
    type Body = S::Body;
    type Future = S::Future;

    fn call(&self, req: http::Request<Incoming>) -> S::Future {
        (**self).call(req)
    }
}

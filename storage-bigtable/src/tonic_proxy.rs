// adapted from hyper-proxy-0.9.1 src/lib.rs

use futures::future::TryFutureExt;
use hyper::{service::Service, Uri};
use hyper_proxy::{Dst, Proxy, ProxyStream};
use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[inline]
pub(crate) fn io_err<E: Into<Box<dyn std::error::Error + Send + Sync>>>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e)
}

#[derive(Clone)]
pub(crate) struct TonicProxyConnector<C> {
    proxies: Vec<Proxy>,
    connector: C,
}

impl<C> TonicProxyConnector<C> {
    pub fn from_proxy(connector: C, proxy: Proxy) -> Result<Self, io::Error> {
        Ok(TonicProxyConnector {
            proxies: vec![proxy],
            connector: connector,
        })
    }

    fn match_proxy<D: Dst>(&self, uri: &D) -> Option<&Proxy> {
        self.proxies.iter().find(|p| p.intercept().matches(uri))
    }
}

macro_rules! mtry {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => break Err(e.into()),
        }
    };
}

impl<C> Service<Uri> for TonicProxyConnector<C>
where
    C: Service<Uri>,
    C::Response: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    C::Future: Send + 'static,
    C::Error: Into<BoxError>,
{
    type Response = ProxyStream<C::Response>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let res = self.connector.poll_ready(cx);
        match res {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(io_err(e.into()))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        if let (Some(p), Some(host)) = (self.match_proxy(&uri), uri.host()) {
            // no force connect
            if uri.scheme() == Some(&http::uri::Scheme::HTTPS) {
                let host = host.to_owned();
                let port = uri.port_u16().unwrap_or(443);
                let tunnel = crate::tonic_tunnel::new(&host, port, &p.headers());
                let connection =
                    proxy_dst(&uri, p.uri()).map(|proxy_url| self.connector.call(proxy_url));
                Box::pin(async move {
                    loop {
                        // this hack will gone once `try_blocks` will eventually stabilized
                        let proxy_stream = mtry!(mtry!(connection).await.map_err(io_err));
                        let tunnel_stream = mtry!(tunnel.with_stream(proxy_stream).await);

                        return Ok(ProxyStream::Regular(tunnel_stream));
                    }
                })
            } else {
                match proxy_dst(&uri, p.uri()) {
                    Ok(proxy_uri) => Box::pin(
                        self.connector
                            .call(proxy_uri)
                            .map_ok(ProxyStream::Regular)
                            .map_err(|err| io_err(err.into())),
                    ),
                    Err(err) => Box::pin(futures::future::err(io_err(err))),
                }
            }
        } else {
            Box::pin(
                self.connector
                    .call(uri)
                    .map_ok(ProxyStream::NoProxy)
                    .map_err(|err| io_err(err.into())),
            )
        }
    }
}

fn proxy_dst(dst: &Uri, proxy: &Uri) -> io::Result<Uri> {
    Uri::builder()
        .scheme(
            proxy
                .scheme_str()
                .ok_or_else(|| io_err(format!("proxy uri missing scheme: {}", proxy)))?,
        )
        .authority(
            proxy
                .authority()
                .ok_or_else(|| io_err(format!("proxy uri missing host: {}", proxy)))?
                .clone(),
        )
        .path_and_query(dst.path_and_query().unwrap().clone())
        .build()
        .map_err(|err| io_err(format!("other error: {}", err)))
}



// Copyright 2019-2021 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any
// person obtaining a copy of this software and associated
// documentation files (the "Software"), to deal in the
// Software without restriction, including without
// limitation the rights to use, copy, modify, merge,
// publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software
// is furnished to do so, subject to the following
// conditions:
//
// The above copyright notice and this permission notice
// shall be included in all copies or substantial portions
// of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
// ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
// TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
// PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
// SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
// CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
// OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
// IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! This example shows how to use the `jsonrpsee::server` as
//! a tower service such that it's possible to get access
//! HTTP related things by launching a `hyper::service_fn`.
//!
//! The typical use-case for this is when one wants to have
//! access to HTTP related things.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::FutureExt;
use hyper::HeaderMap;
use hyper::header::AUTHORIZATION;
use jsonrpsee::core::async_trait;
use jsonrpsee::http_client::HttpClient;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::server::middleware::rpc::{ResponseFuture, RpcServiceBuilder, RpcServiceT};
use jsonrpsee::server::{
	ServerConfig, ServerHandle, StopHandle, TowerServiceBuilder, serve_with_graceful_shutdown, stop_channel,
};
use jsonrpsee::types::{ErrorObject, ErrorObjectOwned, Request};
use jsonrpsee::ws_client::{HeaderValue, WsClientBuilder};
use jsonrpsee::{MethodResponse, Methods};
use tokio::net::TcpListener;
use tower::Service;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Default, Clone, Debug)]
struct Metrics {
	opened_ws_connections: Arc<AtomicUsize>,
	closed_ws_connections: Arc<AtomicUsize>,
	http_calls: Arc<AtomicUsize>,
	success_http_calls: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct AuthorizationMiddleware<S> {
	headers: HeaderMap,
	inner: S,
	#[allow(unused)]
	transport_label: &'static str,
}

impl<'a, S> RpcServiceT<'a> for AuthorizationMiddleware<S>
where
	S: Send + Clone + Sync + RpcServiceT<'a>,
{
	type Future = ResponseFuture<S::Future>;

	fn call(&self, req: Request<'a>) -> Self::Future {
		if req.method_name() == "trusted_call" {
			let Some(Ok(_)) = self.headers.get(AUTHORIZATION).map(|auth| auth.to_str()) else {
				let rp = MethodResponse::error(req.id, ErrorObject::borrowed(-32000, "Authorization failed", None));
				return ResponseFuture::ready(rp);
			};

			// In this example for simplicity, the authorization value is not checked
			// and used because it's just a toy example.

			ResponseFuture::future(self.inner.call(req))
		} else {
			ResponseFuture::future(self.inner.call(req))
		}
	}
}

#[rpc(server, client)]
pub trait Rpc {
	#[method(name = "trusted_call")]
	async fn trusted_call(&self) -> Result<String, ErrorObjectOwned>;
}

#[async_trait]
impl RpcServer for () {
	async fn trusted_call(&self) -> Result<String, ErrorObjectOwned> {
		Ok("mysecret".to_string())
	}
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let filter = tracing_subscriber::EnvFilter::try_from_default_env()?;
	tracing_subscriber::FmtSubscriber::builder().with_env_filter(filter).finish().try_init()?;

	let metrics = Metrics::default();

	let handle = run_server(metrics.clone()).await?;
	tokio::spawn(handle.stopped());

	{
		let client = HttpClient::builder().build("http://127.0.0.1:9944").unwrap();

		// Fails because the authorization header is missing.
		let x = client.trusted_call().await.unwrap_err();
		tracing::info!("response: {x}");
	}

	{
		let client = WsClientBuilder::default().build("ws://127.0.0.1:9944").await.unwrap();

		// Fails because the authorization header is missing.
		let x = client.trusted_call().await.unwrap_err();
		tracing::info!("response: {x}");
	}

	{
		let mut headers = HeaderMap::new();
		headers.insert(AUTHORIZATION, HeaderValue::from_static("don't care in this example"));

		let client = HttpClient::builder().set_headers(headers).build("http://127.0.0.1:9944").unwrap();

		let x = client.trusted_call().await.unwrap();
		tracing::info!("response: {x}");
	}

	tracing::info!("{:?}", metrics);

	Ok(())
}

async fn run_server(metrics: Metrics) -> anyhow::Result<ServerHandle> {
	let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 9944))).await?;

	// This state is cloned for every connection
	// all these types based on Arcs and it should
	// be relatively cheap to clone them.
	//
	// Make sure that nothing expensive is cloned here
	// when doing this or use an `Arc`.
	#[derive(Clone)]
	struct PerConnection<RpcMiddleware, HttpMiddleware> {
		methods: Methods,
		stop_handle: StopHandle,
		metrics: Metrics,
		svc_builder: TowerServiceBuilder<RpcMiddleware, HttpMiddleware>,
	}

	// Each RPC call/connection get its own `stop_handle`
	// to able to determine whether the server has been stopped or not.
	//
	// To keep the server running the `server_handle`
	// must be kept and it can also be used to stop the server.
	let (stop_handle, server_handle) = stop_channel();

	let per_conn = PerConnection {
		methods: ().into_rpc().into(),
		stop_handle: stop_handle.clone(),
		metrics,
		svc_builder: jsonrpsee::server::Server::builder()
			.set_config(ServerConfig::builder().max_connections(33).build())
			.set_http_middleware(tower::ServiceBuilder::new().layer(CorsLayer::permissive()))
			.to_service_builder(),
	};

	tokio::spawn(async move {
		loop {
			// The `tokio::select!` macro is used to wait for either of the
			// listeners to accept a new connection or for the server to be
			// stopped.
			let sock = tokio::select! {
				res = listener.accept() => {
					match res {
						Ok((stream, _remote_addr)) => stream,
						Err(e) => {
							tracing::error!("failed to accept v4 connection: {:?}", e);
							continue;
						}
					}
				}
				_ = per_conn.stop_handle.clone().shutdown() => break,
			};
			let per_conn2 = per_conn.clone();

			let svc = tower::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
				let is_websocket = jsonrpsee::server::ws::is_upgrade_request(&req);
				let transport_label = if is_websocket { "ws" } else { "http" };
				let PerConnection { methods, stop_handle, metrics, svc_builder } = per_conn2.clone();

				let http_middleware = tower::ServiceBuilder::new().layer(CompressionLayer::new());
				// NOTE, the rpc middleware must be initialized here to be able to created once per connection
				// with data from the connection such as the headers in this example
				let headers = req.headers().clone();
				let rpc_middleware = RpcServiceBuilder::new().rpc_logger(1024).layer_fn(move |service| {
					AuthorizationMiddleware { inner: service, headers: headers.clone(), transport_label }
				});

				let mut svc = svc_builder
					.set_http_middleware(http_middleware)
					.set_rpc_middleware(rpc_middleware)
					.build(methods, stop_handle);

				if is_websocket {
					// Utilize the session close future to know when the actual WebSocket
					// session was closed.
					let session_close = svc.on_session_closed();

					// A little bit weird API but the response to HTTP request must be returned below
					// and we spawn a task to register when the session is closed.
					tokio::spawn(async move {
						session_close.await;
						tracing::info!("Closed WebSocket connection");
						metrics.closed_ws_connections.fetch_add(1, Ordering::Relaxed);
					});

					async move {
						tracing::info!("Opened WebSocket connection");
						metrics.opened_ws_connections.fetch_add(1, Ordering::Relaxed);
						// https://github.com/rust-lang/rust/issues/102211 the error type can't be inferred
						// to be `Box<dyn std::error::Error + Send + Sync>` so we need to convert it to a concrete type
						// as workaround.
						svc.call(req).await.map_err(|e| anyhow::anyhow!("{:?}", e))
					}
					.boxed()
				} else {
					// HTTP.
					async move {
						tracing::info!("Opened HTTP connection");
						metrics.http_calls.fetch_add(1, Ordering::Relaxed);
						let rp = svc.call(req).await;

						if rp.is_ok() {
							metrics.success_http_calls.fetch_add(1, Ordering::Relaxed);
						}

						tracing::info!("Closed HTTP connection");
						// https://github.com/rust-lang/rust/issues/102211 the error type can't be inferred
						// to be `Box<dyn std::error::Error + Send + Sync>` so we need to convert it to a concrete type
						// as workaround.
						rp.map_err(|e| anyhow::anyhow!("{:?}", e))
					}
					.boxed()
				}
			});

			tokio::spawn(serve_with_graceful_shutdown(sock, svc, stop_handle.clone().shutdown()));
		}
	});

	Ok(server_handle)
}

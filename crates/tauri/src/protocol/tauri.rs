// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

//! Handler for the `tauri://` custom protocol, serving bundled app assets
//! in production and proxying to the dev server on mobile during development.

use http::{Request, Response as HttpResponse, StatusCode, header::CONTENT_TYPE};
use std::{borrow::Cow, error::Error as StdError, marker::PhantomData, sync::Arc, time::Duration};
use tauri_utils::config::HeaderAddition;

use crate::{
  Manager, Runtime,
  manager::webview::PROXY_DEV_SERVER,
  webview::{UriSchemeProtocolHandler, WebResourceRequestHandler},
};

#[cfg(all(dev, mobile))]
use std::collections::HashMap;
#[cfg(all(dev, mobile))]
use tokio::sync::Mutex;

#[cfg(all(dev, mobile))]
#[derive(Clone)]
struct CachedResponse {
  status: http::StatusCode,
  headers: http::HeaderMap,
  body: bytes::Bytes,
}

/// Creates a URI scheme protocol handler for the `tauri://` custom protocol.
///
/// This handler serves your app's bundled assets (HTML, JS, CSS, etc.) in production,
/// and proxies requests to the dev server on mobile during development.
pub fn get<M: Manager<R> + Send + Sync + 'static, R: Runtime>(
  manager: M,
  window_origin: &str,
  web_resource_request_handler: Option<Box<WebResourceRequestHandler>>,
) -> UriSchemeProtocolHandler {
  let responder = RequestCallbackBuilder::new(manager, window_origin, web_resource_request_handler);
  Arc::new(responder).into_callback()
}

struct RequestCallbackBuilder<M, R> {
  manager: M,
  window_origin: String,
  web_resource_request_handler: Option<Box<WebResourceRequestHandler>>,
  #[cfg(all(dev, mobile))]
  url: String,
  #[cfg(all(dev, mobile))]
  response_cache: Mutex<HashMap<String, CachedResponse>>,
  #[cfg(all(dev, mobile))]
  client: reqwest::Client,
  #[cfg(all(dev, mobile))]
  semaphore: tokio::sync::Semaphore,
  runtime: PhantomData<fn() -> R>,
}

impl<M, R> RequestCallbackBuilder<M, R>
where
  M: Manager<R> + Send + Sync + 'static,
  R: Runtime,
{
  fn new(
    manager: M,
    window_origin: &str,
    web_resource_request_handler: Option<Box<WebResourceRequestHandler>>,
  ) -> Self {
    #[cfg(all(dev, mobile))]
    let response_cache = Mutex::new(HashMap::new());

    #[cfg(all(dev, mobile))]
    let url = {
      let mut url = manager
        .manager()
        .get_app_url(window_origin.starts_with("https"))
        .as_str()
        .to_string();
      if url.ends_with('/') {
        url.pop();
      }
      url
    };

    #[cfg(all(dev, mobile))]
    let client = {
      let mut builder = reqwest::ClientBuilder::new();

      #[cfg(feature = "rustls-tls")]
      if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
      }

      if url.starts_with("https://") {
        if let Some(cert_pem) = option_env!("TAURI_DEV_ROOT_CERTIFICATE") {
          #[cfg(any(
            feature = "native-tls",
            feature = "native-tls-vendored",
            feature = "rustls-tls"
          ))]
          {
            log::info!("adding dev server root certificate");
            let certificate = reqwest::Certificate::from_pem(cert_pem.as_bytes())
              .expect("failed to parse TAURI_DEV_ROOT_CERTIFICATE");
            builder = builder.tls_certs_merge([certificate]);
          }

          #[cfg(not(any(
            feature = "native-tls",
            feature = "native-tls-vendored",
            feature = "rustls-tls"
          )))]
          {
            log::warn!(
              "the dev root-certificate-path option was provided, but you must enable one of the following Tauri features in Cargo.toml: native-tls, native-tls-vendored, rustls-tls"
            );
          }
        } else {
          log::warn!(
            "loading HTTPS URL; you might need to provide a certificate via the `dev --root-certificate-path` option. You must enable one of the following Tauri features in Cargo.toml: native-tls, native-tls-vendored, rustls-tls"
          );
        }
      }

      builder
        .pool_max_idle_per_host(6)
        .build()
        .unwrap()
    };

    Self {
      manager,
      window_origin: window_origin.into(),
      web_resource_request_handler,
      #[cfg(all(dev, mobile))]
      url,
      #[cfg(all(dev, mobile))]
      response_cache,
      #[cfg(all(dev, mobile))]
      client,
      #[cfg(all(dev, mobile))]
      semaphore: tokio::sync::Semaphore::new(6),
      runtime: PhantomData,
    }
  }

  fn into_callback(self: Arc<Self>) -> UriSchemeProtocolHandler {
    Box::new(move |_, request, responder| {
      let this = self.clone();
      crate::async_runtime::spawn(async move {
        let RequestCallbackBuilder {
          manager,
          window_origin,
          web_resource_request_handler,
          #[cfg(all(dev, mobile))]
          url,
          #[cfg(all(dev, mobile))]
          response_cache,
          #[cfg(all(dev, mobile))]
          client,
          #[cfg(all(dev, mobile))]
          semaphore,
          ..
        } = &*this;

        #[cfg(all(dev, mobile))]
        let _permit = semaphore.acquire().await.unwrap();

        let resp_fut = get_response(
          request,
          manager,
          window_origin.as_str(),
          web_resource_request_handler.as_deref(),
          #[cfg(all(dev, mobile))]
          (url.as_str(), response_cache, client),
        );

        match tokio::time::timeout(Duration::from_secs(60), resp_fut).await {
          Ok(Ok(response)) => responder.respond(response),
          Ok(Err(e)) => responder.respond(
            HttpResponse::builder()
              .status(StatusCode::INTERNAL_SERVER_ERROR)
              .header(CONTENT_TYPE, mime::TEXT_PLAIN.essence_str())
              .header("Access-Control-Allow-Origin", window_origin.as_str())
              .body(e.to_string().into_bytes())
              .unwrap(),
          ),
          Err(_) => responder.respond(
            HttpResponse::builder()
              .status(StatusCode::GATEWAY_TIMEOUT)
              .header(CONTENT_TYPE, mime::TEXT_PLAIN.essence_str())
              .header("Access-Control-Allow-Origin", window_origin.as_str())
              .body("request to dev server timed out".as_bytes().to_vec())
              .unwrap(),
          ),
        }
      });
    })
  }
}

async fn get_response<M: Manager<R> + Send + Sync + 'static, R: Runtime>(
  #[allow(unused_mut)] mut request: Request<Vec<u8>>,
  #[allow(unused_variables)] manager: &M,
  window_origin: &str,
  web_resource_request_handler: Option<&WebResourceRequestHandler>,
  #[cfg(all(dev, mobile))] (url, response_cache, client): (&str, &Mutex<HashMap<String, CachedResponse>>, &reqwest::Client),
) -> Result<HttpResponse<Cow<'static, [u8]>>, Box<dyn std::error::Error>> {
  // use the entire URI as we are going to proxy the request
  let path = if PROXY_DEV_SERVER {
    request.uri().to_string()
  } else {
    // ignore query string and fragment
    request
      .uri()
      .to_string()
      .split(&['?', '#'][..])
      .next()
      .unwrap()
      .into()
  };

  let path = path
    .strip_prefix("tauri://localhost")
    .map(|p| p.to_string())
    // the `strip_prefix` only returns None when a request is made to `https://tauri.$P` on Windows and Android
    // where `$P` is not `localhost/*`
    .unwrap_or_default();

  let mut builder = HttpResponse::builder()
    .add_configured_headers(manager.config().app.security.headers.as_ref())
    .header("Access-Control-Allow-Origin", window_origin);

  #[cfg(all(dev, mobile))]
  let mut response = {
    let decoded_path = percent_encoding::percent_decode(path.as_bytes())
      .decode_utf8_lossy()
      .to_string();
    let url = format!(
      "{}/{}",
      url.trim_end_matches('/'),
      decoded_path.trim_start_matches('/')
    );

    let mut proxy_builder = client
      .request(request.method().clone(), &url);
    proxy_builder = proxy_builder.body(std::mem::take(request.body_mut()));
    for (name, value) in request.headers() {
      proxy_builder = proxy_builder.header(name, value);
    }
    proxy_builder = proxy_builder.body(request.body().clone());
    match proxy_builder.send().await {
      Ok(r) => {
        let mut response_cache_ = response_cache.lock().await;
        let mut response = None;
        if r.status() == http::StatusCode::NOT_MODIFIED {
          response = response_cache_.get(&url);
        }
        let response = if let Some(r) = response {
          r
        } else {
          let status = r.status();
          let headers = r.headers().clone();
          let body = r.bytes().await?;
          let response = CachedResponse {
            status,
            headers,
            body,
          };
          response_cache_.insert(url.clone(), response);
          response_cache_.get(&url).unwrap()
        };
        for (name, value) in &response.headers {
          builder = builder.header(name, value);
        }
        builder
          .status(response.status)
          .body(response.body.to_vec().into())?
      }
      Err(e) => {
        let is_connect = e.is_connect();
        let is_timeout = e.is_timeout();
        let is_request = e.is_request();
        let source_chain = {
          let mut chain = Vec::new();
          let mut source: Option<&dyn StdError> = StdError::source(&e);
          while let Some(s) = source {
            chain.push(format!("{s}"));
            source = s.source();
          }
          chain.join(" -> ")
        };
        let error_message = format!(
          "Failed to request {url}: {e} [connect={is_connect}, timeout={is_timeout}, request={is_request}, thread={:?}, sources: {source_chain}]",
          std::thread::current().name(),
        );
        log::error!("{error_message}");
        return Err(error_message.into());
      }
    }
  };

  #[cfg(not(all(dev, mobile)))]
  let mut response = {
    let use_https_scheme = request.uri().scheme() == Some(&http::uri::Scheme::HTTPS);
    let asset = manager.manager().get_asset(path, use_https_scheme)?;
    builder = builder.header(CONTENT_TYPE, &asset.mime_type);
    if let Some(csp) = &asset.csp_header {
      builder = builder.header("Content-Security-Policy", csp);
    }
    builder.body(asset.bytes.into())?
  };
  if let Some(handler) = &web_resource_request_handler {
    handler(request, &mut response);
  }

  Ok(response)
}

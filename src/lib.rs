//! Provides wrappers for [`reqwest`][2] to enable [websocket][1] connections.
//!
//! # Example
//!
//! ```
//! # use reqwest::Client;
//! # use reqwest_websocket::Message;
//! # use futures_util::{TryStreamExt, SinkExt};
//! # fn main() {
//! # run(); // intentionally ignore the future. we only care that it compiles.
//! # }
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! // Extends the reqwest::RequestBuilder to allow websocket upgrades
//! use reqwest_websocket::RequestBuilderExt;
//!
//! // create a GET request, upgrade it and send it.
//! let response = Client::default()
//!     .get("wss://echo.websocket.org/")
//!     .upgrade() // prepares the websocket upgrade.
//!     .send()
//!     .await?;
//!
//! // turn the response into a websocket stream
//! let mut websocket = response.into_websocket().await?;
//!
//! // the websocket implements `Sink<Message>`.
//! websocket.send(Message::Text("Hello, World".into())).await?;
//!
//! // the websocket is also a `TryStream` over `Message`s.
//! while let Some(message) = websocket.try_next().await? {
//!     match message {
//!         Message::Text(text) => println!("{text}"),
//!         _ => {}
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! [1]: https://en.wikipedia.org/wiki/WebSocket
//! [2]: https://docs.rs/reqwest/latest/reqwest/index.html

#[cfg(not(target_arch = "wasm32"))]
mod native;
mod protocol;
#[cfg(target_arch = "wasm32")]
mod wasm;

use std::{
    pin::Pin,
    task::{
        Context,
        Poll,
    },
};

use futures_util::{
    Sink,
    SinkExt,
    Stream,
    StreamExt,
};
#[cfg(not(target_arch = "wasm32"))]
pub use native::HandshakeError;
pub use protocol::{
    CloseCode,
    Message,
};
use reqwest::{
    Client,
    ClientBuilder,
    IntoUrl,
    RequestBuilder,
};

/// Errors returned by `reqwest_websocket`
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[cfg(not(target_arch = "wasm32"))]
    #[error("websocket upgrade failed")]
    Handshake(#[from] native::HandshakeError),

    #[error("reqwest error")]
    Reqwest(#[from] reqwest::Error),

    #[cfg(not(target_arch = "wasm32"))]
    #[error("tungstenite error")]
    Tungstenite(#[from] tungstenite::Error),

    #[cfg(target_arch = "wasm32")]
    #[error("web_sys error")]
    WebSys(#[from] wasm::WebSysError),
}

/// Opens a websocket at the specified URL.
///
/// This is a shorthand for creating a request, sending it, and turning the
/// response into a websocket.
pub async fn websocket(url: impl IntoUrl) -> Result<WebSocket, Error> {
    let builder = builder_http1_only(Client::builder());
    Ok(builder
        .build()?
        .get(url)
        .upgrade()
        .send()
        .await?
        .into_websocket()
        .await?)
}

#[cfg(not(target_arch = "wasm32"))]
fn builder_http1_only(builder: ClientBuilder) -> ClientBuilder {
    builder.http1_only()
}

#[cfg(target_arch = "wasm32")]
fn builder_http1_only(builder: ClientBuilder) -> ClientBuilder {
    builder
}

/// Trait that extends [`reqwest::RequestBuilder`] with an `upgrade` method.
pub trait RequestBuilderExt {
    fn upgrade(self) -> UpgradedRequestBuilder;
}

impl RequestBuilderExt for RequestBuilder {
    /// Upgrades the [`RequestBuilder`] to peform a
    /// websocket handshake. This returns a wrapped type, so you you must do
    /// this after you setup your request, and just before you send the
    /// request.
    fn upgrade(self) -> UpgradedRequestBuilder {
        UpgradedRequestBuilder::new(self)
    }
}

/// Wrapper for [`RequestBuilder`] that performs the
/// websocket handshake when sent.
pub struct UpgradedRequestBuilder {
    inner: RequestBuilder,
    protocols: Vec<String>,
}

impl UpgradedRequestBuilder {
    pub(crate) fn new(inner: RequestBuilder) -> Self {
        Self {
            inner,
            protocols: vec![],
        }
    }

    /// Sends the request and returns an [`UpgradeResponse`].
    pub async fn send(self) -> Result<UpgradeResponse, Error> {
        #[cfg(not(target_arch = "wasm32"))]
        let inner = native::send_request(self.inner).await?;

        #[cfg(target_arch = "wasm32")]
        let inner = wasm::WebSysWebSocketStream::new(self.inner.build()?, &self.protocols).await?;

        Ok(UpgradeResponse {
            inner,
            protocols: self.protocols,
        })
    }
}

/// The server's response to the websocket upgrade request.
///
/// On non-wasm platforms, this implements `Deref<Target = Response>`, so you
/// can access all the usual information from the [`reqwest::Response`].
pub struct UpgradeResponse {
    #[cfg(not(target_arch = "wasm32"))]
    inner: native::WebSocketResponse,

    #[cfg(target_arch = "wasm32")]
    inner: wasm::WebSysWebSocketStream,

    #[allow(dead_code)]
    protocols: Vec<String>,
}

#[cfg(not(target_arch = "wasm32"))]
impl std::ops::Deref for UpgradeResponse {
    type Target = reqwest::Response;

    fn deref(&self) -> &Self::Target {
        &self.inner.response
    }
}

impl UpgradeResponse {
    /// Turns the response into a websocket. This checks if the websocket
    /// handshake was successful.
    pub async fn into_websocket(self) -> Result<WebSocket, Error> {
        #[cfg(not(target_arch = "wasm32"))]
        let (inner, protocol) = self.inner.into_stream_and_protocol(self.protocols).await?;

        #[cfg(target_arch = "wasm32")]
        let (inner, protocol) = {
            let protocol = self.inner.protocol();
            (self.inner, Some(protocol))
        };

        Ok(WebSocket { inner, protocol })
    }

    /// Consumes the response and returns the inner [`reqwest::Response`].
    #[cfg(not(target_arch = "wasm32"))]
    pub fn into_inner(self) -> reqwest::Response {
        self.inner.response
    }
}

/// A websocket connection
pub struct WebSocket {
    #[cfg(not(target_arch = "wasm32"))]
    inner: native::WebSocketStream,

    #[cfg(target_arch = "wasm32")]
    inner: wasm::WebSysWebSocketStream,

    protocol: Option<String>,
}

impl WebSocket {
    pub fn protocol(&self) -> Option<&str> {
        self.protocol.as_deref()
    }

    pub async fn close(self, code: CloseCode, reason: Option<&str>) -> Result<(), Error> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut inner = self.inner;
            inner
                .close(Some(tungstenite::protocol::CloseFrame {
                    code: code.into(),
                    reason: reason.unwrap_or_default().into(),
                }))
                .await?;
        }

        #[cfg(target_arch = "wasm32")]
        self.inner.close(code.into(), reason.unwrap_or_default())?;

        Ok(())
    }
}

impl Stream for WebSocket {
    type Item = Result<Message, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.inner.poll_next_unpin(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Some(Err(error.into()))),
                Poll::Ready(Some(Ok(message))) => {
                    match message.try_into() {
                        Ok(message) => return Poll::Ready(Some(Ok(message))),
                        Err(_) => {
                            // this won't convert pings, pongs, etc. but we
                            // don't care about those.
                        }
                    }
                }
            }
        }
    }
}

impl Sink<Message> for WebSocket {
    type Error = Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready_unpin(cx).map_err(Into::into)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        self.inner.start_send_unpin(item.into()).map_err(Into::into)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_flush_unpin(cx).map_err(Into::into)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_close_unpin(cx).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use futures_util::{
        SinkExt,
        TryStreamExt,
    };
    use reqwest::Client;

    use super::{
        websocket,
        CloseCode,
        Message,
        RequestBuilderExt,
        WebSocket,
    };

    async fn test_websocket(mut websocket: WebSocket) {
        let text = "Hello, World!";
        websocket
            .send(Message::Text(text.to_owned()))
            .await
            .unwrap();

        while let Some(message) = websocket.try_next().await.unwrap() {
            match message {
                Message::Text(s) => {
                    if s == text {
                        return;
                    }
                }
                _ => {}
            }
        }

        panic!("didn't receive text back");
    }

    #[tokio::test]
    async fn test_with_request_builder() {
        let websocket = Client::default()
            .get("https://echo.websocket.org/")
            .upgrade()
            .send()
            .await
            .unwrap()
            .into_websocket()
            .await
            .unwrap();

        test_websocket(websocket).await;
    }

    #[tokio::test]
    async fn test_shorthand() {
        let websocket = websocket("https://echo.websocket.org/").await.unwrap();
        test_websocket(websocket).await;
    }

    #[tokio::test]
    async fn test_with_ws_scheme() {
        let websocket = websocket("wss://echo.websocket.org/").await.unwrap();

        test_websocket(websocket).await;
    }

    #[tokio::test]
    async fn test_close() {
        let websocket = websocket("https://echo.websocket.org/").await.unwrap();
        websocket.close(CloseCode::Protocol, Some("test")).await.expect("close returned an error");
    }

    #[test]
    fn closecode_from_u16() {
        let byte = 1008u16;
        assert_eq!(CloseCode::from(byte), CloseCode::Policy);
    }

    #[test]
    fn closecode_into_u16() {
        let text = CloseCode::Away;
        let byte: u16 = text.into();
        assert_eq!(byte, 1001u16);
        assert_eq!(u16::from(text), 1001u16);
    }
}

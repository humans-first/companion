mod client;
mod server;
#[cfg(test)]
mod tests;

use std::pin::Pin;

use futures::future::BoxFuture;
use futures::{AsyncRead, AsyncWrite};

pub use client::{connect, ClientConfig};
pub use server::{serve, ConnectionFactory, HttpTransportConfig, ServerTlsConfig};

pub type BoxAsyncRead = Pin<Box<dyn AsyncRead + Send + 'static>>;
pub type BoxAsyncWrite = Pin<Box<dyn AsyncWrite + Send + 'static>>;

pub struct TransportPeer {
    pub reader: BoxAsyncRead,
    pub writer: BoxAsyncWrite,
    pub shutdown: BoxFuture<'static, ()>,
}

impl TransportPeer {
    pub fn new(
        reader: impl AsyncRead + Send + 'static,
        writer: impl AsyncWrite + Send + 'static,
        shutdown: BoxFuture<'static, ()>,
    ) -> Self {
        Self {
            reader: Box::pin(reader),
            writer: Box::pin(writer),
            shutdown,
        }
    }
}

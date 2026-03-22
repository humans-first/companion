mod client;
mod server;
#[cfg(test)]
mod tests;

use std::pin::Pin;

use futures::future::LocalBoxFuture;
use futures::{AsyncRead, AsyncWrite};

pub use client::{connect, ClientConfig};
pub use server::{serve, ConnectionFactory, HttpTransportConfig};

pub type BoxAsyncRead = Pin<Box<dyn AsyncRead + 'static>>;
pub type BoxAsyncWrite = Pin<Box<dyn AsyncWrite + 'static>>;

pub struct TransportPeer {
    pub reader: BoxAsyncRead,
    pub writer: BoxAsyncWrite,
    pub shutdown: LocalBoxFuture<'static, ()>,
}

impl TransportPeer {
    pub fn new(
        reader: impl AsyncRead + 'static,
        writer: impl AsyncWrite + 'static,
        shutdown: LocalBoxFuture<'static, ()>,
    ) -> Self {
        Self {
            reader: Box::pin(reader),
            writer: Box::pin(writer),
            shutdown,
        }
    }
}

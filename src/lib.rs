#[macro_use] extern crate log;

use std::{
    panic::AssertUnwindSafe,
    convert::TryFrom,
    io::BufReader,
    sync::Arc,
    time::Duration,
};
use futures_core::future::BoxFuture;
use tokio::{
    prelude::*,
    io::{self, BufStream},
    net::{TcpStream, ToSocketAddrs},
    time::timeout,
};
use tokio::net::TcpListener;
use rustls::ClientCertVerifier;
use tokio_rustls::{rustls, TlsAcceptor};
use rustls::*;
use anyhow::*;
use lazy_static::lazy_static;

pub mod types;
pub mod util;

pub use mime;
pub use uriparse as uri;
pub use types::*;

pub const REQUEST_URI_MAX_LEN: usize = 1024;
pub const GEMINI_PORT: u16 = 1965;

type Handler = Arc<dyn Fn(Request) -> HandlerResponse + Send + Sync>;
pub (crate) type HandlerResponse = BoxFuture<'static, Result<Response>>;

#[derive(Clone)]
pub struct Server {
    tls_acceptor: TlsAcceptor,
    listener: Arc<TcpListener>,
    handler: Handler,
    timeout: Duration,
}

impl Server {
    pub fn bind<A: ToSocketAddrs>(addr: A) -> Builder<A> {
        Builder::bind(addr)
    }

    async fn serve(self) -> Result<()> {
        loop {
            let (stream, _addr) = self.listener.accept().await
                .context("Failed to accept client")?;
            let this = self.clone();

            tokio::spawn(async move {
                if let Err(err) = this.serve_client(stream).await {
                    error!("{:?}", err);
                }
            });
        }
    }

    async fn serve_client(self, stream: TcpStream) -> Result<()> {
        let fut_accept_request = async {
            let stream = self.tls_acceptor.accept(stream).await
                .context("Failed to establish TLS session")?;
            let mut stream = BufStream::new(stream);

            let request = receive_request(&mut stream).await
                .context("Failed to receive request")?;

            Result::<_, anyhow::Error>::Ok((request, stream))
        };

        // Use a timeout for interacting with the client
        let fut_accept_request = timeout(self.timeout, fut_accept_request);
        let (mut request, mut stream) = fut_accept_request.await
            .context("Client timed out while waiting for response")??;

        debug!("Client requested: {}", request.uri());

        // Identify the client certificate from the tls stream.  This is the first
        // certificate in the certificate chain.
        let client_cert = stream.get_ref()
            .get_ref()
            .1
            .get_peer_certificates()
            .and_then(|mut v| if v.is_empty() {None} else {Some(v.remove(0))});

        request.set_cert(client_cert);

        let handler = (self.handler)(request);
        let handler = AssertUnwindSafe(handler);

        let response = util::HandlerCatchUnwind::new(handler).await
            .unwrap_or_else(|_| Response::server_error(""))
            .or_else(|err| {
                error!("Handler failed: {:?}", err);
                Response::server_error("")
            })
            .context("Request handler failed")?;

        // Use a timeout for sending the response
        let fut_send_and_flush = async {
            send_response(response, &mut stream).await
                .context("Failed to send response")?;

            stream.flush()
                .await
                .context("Failed to flush response data")
        };
        timeout(self.timeout, fut_send_and_flush)
            .await
            .context("Client timed out receiving response data")??;

        Ok(())
    }
}

pub struct Builder<A> {
    addr: A,
    timeout: Duration,
}

impl<A: ToSocketAddrs> Builder<A> {
    fn bind(addr: A) -> Self {
        Self { addr, timeout: Duration::from_secs(30) }
    }

    /// Set the timeout on incoming requests
    ///
    /// Note that this timeout is applied twice, once for the delivery of the request, and
    /// once for sending the client's response.  This means that for a 1 second timeout,
    /// the client will have 1 second to complete the TLS handshake and deliver a request
    /// header, then your API will have as much time as it needs to handle the request,
    /// before the client has another second to receive the response.
    ///
    /// If you would like a timeout for your code itself, please use
    /// [`tokio::time::Timeout`] to implement it internally.
    ///
    /// The default timeout is 30 seconds.
    pub fn set_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub async fn serve<F>(self, handler: F) -> Result<()>
    where
        F: Fn(Request) -> HandlerResponse + Send + Sync + 'static,
    {
        let config = tls_config()
            .context("Failed to create TLS config")?;

        let listener = TcpListener::bind(self.addr).await
            .context("Failed to create socket")?;

        let server = Server {
            tls_acceptor: TlsAcceptor::from(config),
            listener: Arc::new(listener),
            handler: Arc::new(handler),
            timeout: self.timeout,
        };

        server.serve().await
    }
}

async fn receive_request(stream: &mut (impl AsyncBufRead + Unpin)) -> Result<Request> {
    let limit = REQUEST_URI_MAX_LEN + "\r\n".len();
    let mut stream = stream.take(limit as u64);
    let mut uri = Vec::new();

    stream.read_until(b'\n', &mut uri).await?;

    if !uri.ends_with(b"\r\n") {
        if uri.len() < REQUEST_URI_MAX_LEN {
            bail!("Request header not terminated with CRLF")
        } else {
            bail!("Request URI too long")
        }
    }

    // Strip CRLF
    uri.pop();
    uri.pop();

    let uri = URIReference::try_from(&*uri)
        .context("Request URI is invalid")?
        .into_owned();
    let request = Request::from_uri(uri)
        .context("Failed to create request from URI")?;

    Ok(request)
}

async fn send_response(mut response: Response, stream: &mut (impl AsyncWrite + Unpin)) -> Result<()> {
    send_response_header(response.header(), stream).await
        .context("Failed to send response header")?;

    if let Some(body) = response.take_body() {
        send_response_body(body, stream).await
            .context("Failed to send response body")?;
    }

    Ok(())
}

async fn send_response_header(header: &ResponseHeader, stream: &mut (impl AsyncWrite + Unpin)) -> Result<()> {
    let header = format!(
        "{status} {meta}\r\n",
        status = header.status.code(),
        meta = header.meta.as_str(),
    );

    stream.write_all(header.as_bytes()).await?;

    Ok(())
}

async fn send_response_body(body: Body, stream: &mut (impl AsyncWrite + Unpin)) -> Result<()> {
    match body {
        Body::Bytes(bytes) => stream.write_all(&bytes).await?,
        Body::Reader(mut reader) => { io::copy(&mut reader, stream).await?; },
    }

    Ok(())
}

fn tls_config() -> Result<Arc<ServerConfig>> {
    let mut config = ServerConfig::new(AllowAnonOrSelfsignedClient::new());

    let cert_chain = load_cert_chain()
        .context("Failed to load TLS certificate")?;
    let key = load_key()
        .context("Failed to load TLS key")?;
    config.set_single_cert(cert_chain, key)
        .context("Failed to use loaded TLS certificate")?;

    Ok(config.into())
}

fn load_cert_chain() -> Result<Vec<Certificate>> {
    let cert_path = "cert/cert.pem";
    let certs = std::fs::File::open(cert_path)
        .with_context(|| format!("Failed to open `{}`", cert_path))?;
    let mut certs = BufReader::new(certs);
    let certs = rustls::internal::pemfile::certs(&mut certs)
        .map_err(|_| anyhow!("failed to load certs `{}`", cert_path))?;

    Ok(certs)
}

fn load_key() -> Result<PrivateKey> {
    let key_path = "cert/key.pem";
    let keys = std::fs::File::open(key_path)
        .with_context(|| format!("Failed to open `{}`", key_path))?;
    let mut keys = BufReader::new(keys);
    let mut keys = rustls::internal::pemfile::pkcs8_private_keys(&mut keys)
        .map_err(|_| anyhow!("failed to load key `{}`", key_path))?;

    ensure!(!keys.is_empty(), "no key found");

    let key = keys.swap_remove(0);

    Ok(key)
}

/// Mime for Gemini documents
pub const GEMINI_MIME_STR: &str = "text/gemini";

lazy_static! {
    /// Mime for Gemini documents ("text/gemini")
    pub static ref GEMINI_MIME: Mime = GEMINI_MIME_STR.parse().expect("northstar BUG");
}

#[deprecated(note = "Use `GEMINI_MIME` instead", since = "0.3.0")]
pub fn gemini_mime() -> Result<Mime> {
    Ok(GEMINI_MIME.clone())
}

/// A client cert verifier that accepts all connections
///
/// Unfortunately, rustls doesn't provide a ClientCertVerifier that accepts self-signed
/// certificates, so we need to implement this ourselves.
struct AllowAnonOrSelfsignedClient { }
impl AllowAnonOrSelfsignedClient {

    /// Create a new verifier
    fn new() -> Arc<Self> {
        Arc::new(Self {})
    }

}

impl ClientCertVerifier for AllowAnonOrSelfsignedClient {

    fn client_auth_root_subjects(
        &self,
        _: Option<&webpki::DNSName>
    ) -> Option<DistinguishedNames> {
        Some(Vec::new())
    }

    fn client_auth_mandatory(&self, _sni: Option<&webpki::DNSName>) -> Option<bool> {
        Some(false)
    }

    fn verify_client_cert(
        &self,
        _: &[Certificate],
        _: Option<&webpki::DNSName>
    ) -> Result<ClientCertVerified, TLSError> {
        Ok(ClientCertVerified::assertion())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_mime_parses() {
        let _: &Mime = &GEMINI_MIME;
    }
}

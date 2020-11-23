//! # Ngrok
//!
//! A minimal and concise [`ngrok`](https://ngrok.com/) wrapper for Rust. The main use case for the library
//! is the ability to open public HTTP tunnels to your development server(s) for
//! integrations tests. TCP support, while not available, should be trivial to support.
//!
//! This has been tested with Linux and assume that it does not work on Windows (contributions
//! welcome).
//!
//! ## Usage
//! ```
//! fn main() -> std::io::Result<()> {
//!     let tunnel = ngrok::builder()
//!           // server protocol
//!           .http()
//!           // the port
//! #         .executable("./ngrok")
//!           .port(3030)
//!           .run()?;
//!
//!     let public_url = tunnel.http()?;
//!
//!     Ok(())
//! }
//! ```

use serde::Deserialize;
use std::process::Child;
use std::sync::Arc;
use std::sync::Mutex;
use std::{fmt, io, process::Command, process::Stdio, thread, time::Duration, time::Instant};
use thiserror::Error;
use url::Url;

// NGROK JSON API types
#[derive(Debug, Deserialize)]
struct GetTunnels {
    tunnels: Vec<ApiTunnel>,
}

#[derive(Debug, Clone, Deserialize)]
struct Config {
    addr: Url,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiTunnel {
    config: Config,
    public_url: Url,
}

#[derive(Error, Debug)]
enum Error {
    #[error("Expected a matching tunnel but found none under `ngrok`'s JSON API @ http://localhost:4040/api/tunnels")]
    TunnelNotFound,

    #[error("Builder expected `{0}`")]
    BuilderError(&'static str),

    #[error("Tunnel exited unexpectedly with exit status `{0}`")]
    TunnelProcessExited(String),
}

impl From<Error> for io::Error {
    fn from(err: Error) -> Self {
        io::Error::new(io::ErrorKind::Other, err)
    }
}

type Resource = Arc<Mutex<Child>>;

/// A running `ngrok` Tunnel.
#[derive(Debug, Clone)]
pub struct Tunnel {
    pub(crate) proc: Resource,
    /// The tunnel's public URL
    tunnel_http: url::Url,
    /// The tunnel's public URL
    tunnel_https: url::Url,
}

impl AsRef<url::Url> for Tunnel {
    fn as_ref(&self) -> &url::Url {
        &self.tunnel_http
    }
}

impl fmt::Display for Tunnel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.tunnel_http.fmt(f)
    }
}

impl Tunnel {
    /// Build a new `ngrok` Tunnel
    pub fn builder() -> Builder {
        crate::builder()
    }

    /// Determine if the underlying child process has exited
    /// and return the exit error if so.
    pub fn status(&self) -> Result<(), io::Error> {
        let status = { self.proc.lock().unwrap().try_wait()? };

        status
            .map(|code| Err(Error::TunnelProcessExited(code.to_string())))
            .unwrap_or(Ok(()))?;

        Ok(())
    }

    /// Retrieve the tunnel's http URL. If the underlying process has terminated,
    /// this will return the exit status
    pub fn http(&self) -> Result<&Url, io::Error> {
        self.status()?;
        Ok(&self.tunnel_http)
    }

    /// Retrieve the tunnel's https URL. If the underlying process has terminated,
    /// this will return the exit status
    pub fn https(&self) -> Result<&Url, io::Error> {
        self.status()?;
        Ok(&self.tunnel_https)
    }

    /// Retrieve the tunnel's http URL.
    pub fn http_unchecked(&self) -> &Url {
        &self.tunnel_http
    }

    /// Retrieve the tunnel's https URL.
    pub fn https_unchecked(&self) -> &Url {
        &self.tunnel_https
    }
}

impl Drop for Tunnel {
    /// Stop the Ngrok child process
    fn drop(&mut self) {
        let _result = self.proc.lock().unwrap().kill();
    }
}

/// Build a `ngrok` Tunnel. Use `ngrok::builder()` to create this.
#[derive(Debug, Clone, Default)]
pub struct Builder {
    http: Option<()>,
    port: Option<u16>,
    executable: Option<String>,
}

/// The entry point for starting a `ngrok` tunnel. Only HTTP is currently supported.
///
/// **Example**
///
/// ```
/// ngrok::builder()
///         .executable("ngrok")
///         .http()
///         .port(3030)
///         .run()
///         .unwrap();
/// ```
pub fn builder() -> Builder {
    Builder {
        ..Default::default()
    }
}

impl Builder {
    /// Create a new `Builder`
    pub fn new() -> Self {
        Builder {
            ..Default::default()
        }
    }

    /// Set the tunnel protocol to HTTP
    pub fn http(&mut self) -> Self {
        self.http = Some(());
        self.clone()
    }

    /// Set the tunnel port
    pub fn port(&mut self, port: u16) -> Self {
        self.port = Some(port);
        self.clone()
    }

    /// Set the `ngrok` executable path. By default the builder
    /// assumes `ngrok` is on your path.
    pub fn executable(&mut self, executable: &str) -> Self {
        self.executable = Some(executable.to_string());
        self.clone()
    }

    /// Start the `ngrok` child process. Note this is a blocking call
    /// and it will sleep for several seconds.
    // There is a detached thread that waits for either
    // A: the Ngrok instance to drop, which in `impl Drop` sends a message over
    // the channel, or
    // B: the underlying process to quit
    pub fn run(self) -> Result<Tunnel, io::Error> {
        // Prepare for TCP/other
        let _http = self
            .http
            .ok_or(Error::BuilderError(".http() should have been called"))?;

        let port = self
            .port
            .ok_or(Error::BuilderError(".port(port) should have been set"))?;

        let started_at = Instant::now();

        // Start the `ngrok` process
        let proc = Command::new(self.executable.unwrap_or_else(|| "ngrok".to_string()))
            .stdout(Stdio::piped())
            .arg("http")
            .arg(port.to_string())
            .spawn()?;

        // ngrok takes a bit to start up and this is a (probably bad) way to wait
        // for the tunnel to appear:
        let (tunnel_http, tunnel_https) = {
            loop {
                let tunnels = find_tunnels(port);

                if tunnels.is_ok() {
                    break tunnels;
                }

                // If 5 seconds have elapsed, mission failed
                if started_at.elapsed().as_secs() > 5 {
                    break tunnels;
                }

                // Elsewise try again in 300 millis
                thread::sleep(Duration::from_millis(300));
            }
        }?;

        Ok(Tunnel {
            tunnel_http,
            tunnel_https,
            proc: Arc::new(Mutex::new(proc)),
        })
    }
}

fn find_tunnels(port: u16) -> Result<(url::Url, url::Url), io::Error> {
    // Retrieve the `tunnel_url`
    let response = ureq::get("http://localhost:4040/api/tunnels")
        .call()
        .into_json()?;

    let response: GetTunnels = serde_json::from_value(response)?;

    // snag both HTTP/HTTPS urls
    fn find_tunnel_url<I: IntoIterator<Item = ApiTunnel>>(
        scheme: &'static str,
        port: u16,
        iter: I,
    ) -> Result<url::Url, Error> {
        iter.into_iter()
            .find(|tunnel| match tunnel.config.addr.port() {
                Some(p) => p == port && tunnel.public_url.scheme() == scheme,
                None => false,
            })
            .map(|t| Ok(t.public_url))
            .unwrap_or(Err(Error::TunnelNotFound))
    }

    let tunnel_http = find_tunnel_url("http", port, response.tunnels.clone())?;
    let tunnel_https = find_tunnel_url("https", port, response.tunnels)?;

    Ok((tunnel_http, tunnel_https))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_status_if_proc_killed() {
        let tunnel = builder().http().port(3000).run().unwrap();
        tunnel.proc.lock().unwrap().kill().unwrap();
        std::thread::sleep(Duration::from_millis(2500));
        assert!(tunnel.http().is_err())
    }
}

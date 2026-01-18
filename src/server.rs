use crate::{
    error::Error,
    handler::HttpHandler,
    header::HttpHeader,
    request::HttpRequest,
    response::{HttpResponse, ResponseBody},
    status_code::StatusCode,
};
use embassy_net::{Stack, tcp::TcpSocket};
use embassy_time::{Duration, Timer, with_timeout};
use embedded_io_async::Write as EmbeddedWrite;
use heapless::Vec;

const SERVER_BUFFER_SIZE: usize = 4096;
const MAX_REQUEST_SIZE: usize = 4096;
const DEFAULT_MAX_RESPONSE_SIZE: usize = 4096;

/// HTTP server timeout configuration
#[derive(Debug, Clone, Copy)]
pub struct ServerTimeouts {
    /// Socket accept timeout in seconds
    pub accept_timeout: u64,
    /// Socket read timeout in seconds  
    pub read_timeout: u64,
    /// Request handler timeout in seconds
    pub handler_timeout: u64,
}

impl Default for ServerTimeouts {
    fn default() -> Self {
        Self {
            accept_timeout: 10,
            read_timeout: 30,
            handler_timeout: 60,
        }
    }
}

impl ServerTimeouts {
    /// Create new server timeouts with custom values
    #[must_use]
    pub fn new(accept_timeout: u64, read_timeout: u64, handler_timeout: u64) -> Self {
        Self {
            accept_timeout,
            read_timeout,
            handler_timeout,
        }
    }
}

/// Simple HTTP server implementation
///
/// **Note**: This server only supports HTTP connections, not HTTPS/TLS.
/// For secure connections, consider using a reverse proxy or load balancer
/// that handles TLS termination.
pub struct HttpServer<
    const RX_SIZE: usize,
    const TX_SIZE: usize,
    const REQ_SIZE: usize,
    const MAX_RESPONSE_SIZE: usize,
> {
    port: u16,
    timeouts: ServerTimeouts,
}

impl<
    const RX_SIZE: usize,
    const TX_SIZE: usize,
    const REQ_SIZE: usize,
    const MAX_RESPONSE_SIZE: usize,
> HttpServer<RX_SIZE, TX_SIZE, REQ_SIZE, MAX_RESPONSE_SIZE>
{
    /// Create a new HTTP server with default timeouts
    #[must_use]
    pub fn new(port: u16) -> Self {
        Self {
            port,
            timeouts: ServerTimeouts::default(),
        }
    }

    /// Create a new HTTP server with custom timeouts
    #[must_use]
    pub fn with_timeouts(port: u16, timeouts: ServerTimeouts) -> Self {
        Self { port, timeouts }
    }

    /// Start the HTTP server and handle incoming connections
    ///
    /// **Important**: This server only accepts plain HTTP connections.
    /// HTTPS/TLS is not supported by the server (only by the client).
    pub async fn serve<H>(&mut self, stack: Stack<'_>, mut handler: H) -> !
    where
        H: HttpHandler,
    {
        defmt::info!("HTTP server started on port {}", self.port);

        let mut rx_buffer = [0; RX_SIZE];
        let mut tx_buffer = [0; TX_SIZE];
        let mut buf = [0; REQ_SIZE];

        loop {
            let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
            socket.set_timeout(Some(Duration::from_secs(self.timeouts.accept_timeout)));

            if let Err(e) = socket.accept(self.port).await {
                defmt::warn!("Accept error: {:?}", e);
                Timer::after(Duration::from_millis(100)).await;
                continue;
            }

            loop {
                let n = match with_timeout(
                    Duration::from_secs(self.timeouts.read_timeout),
                    socket.read(&mut buf),
                )
                .await
                {
                    Ok(Ok(0)) => {
                        // Connection closed by peer
                        break;
                    }
                    Ok(Ok(n)) => n,
                    Ok(Err(e)) => {
                        defmt::warn!("Read error: {:?}", e);
                        break;
                    }
                    Err(_) => {
                        defmt::warn!("Socket read timeout");
                        break;
                    }
                };

                let should_close = match handle_connection::<MAX_RESPONSE_SIZE, _>(
                    self.timeouts,
                    &buf[..n],
                    &mut handler,
                )
                .await
                {
                    Ok((response_bytes, should_close)) => {
                        if let Err(e) = socket.write_all(&response_bytes).await {
                            defmt::warn!("Failed to write response: {:?}", e);
                            true
                        } else {
                            should_close
                        }
                    }
                    Err(e) => {
                        defmt::error!("Error handling request: {:?}", e);
                        let error_response = b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nContent-Length: 21\r\n\r\nInternal Server Error";
                        let _ = socket.write_all(error_response).await;
                        true
                    }
                };

                if let Err(e) = socket.flush().await {
                    defmt::warn!("Failed to flush response: {:?}", e);
                }

                if should_close {
                    break;
                }
            }

            socket.close();
        }
    }

}

async fn handle_connection<const MAX_RESPONSE_SIZE: usize, H: HttpHandler>(
    timeouts: ServerTimeouts,
    buffer: &[u8],
    handler: &mut H,
) -> Result<(Vec<u8, MAX_RESPONSE_SIZE>, bool), Error> {
    let request = HttpRequest::try_from(buffer)?;
    let should_close = should_close_connection(&request);

    let response = match with_timeout(
        Duration::from_secs(timeouts.handler_timeout),
        handler.handle_request(&request),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => {
            defmt::warn!("Handler error: {:?}", e);
            let mut headers = Vec::new();
            let _ = headers.push(HttpHeader::new("Content-Type", "text/plain"));
            let error_response = HttpResponse {
                status_code: StatusCode::InternalServerError,
                headers,
                body: ResponseBody::Text("Internal Server Error"),
            };
            return Ok((error_response.build_bytes::<MAX_RESPONSE_SIZE>(), true));
        }
        Err(_) => {
            defmt::warn!("Request handling timed out");
            let mut headers = Vec::new();
            let _ = headers.push(HttpHeader::new("Content-Type", "text/plain"));
            let timeout_response = HttpResponse {
                status_code: StatusCode::RequestTimeout,
                headers,
                body: ResponseBody::Text("Request Timeout"),
            };
            return Ok((timeout_response.build_bytes::<MAX_RESPONSE_SIZE>(), true));
        }
    };

    Ok((response.build_bytes::<MAX_RESPONSE_SIZE>(), should_close))
}

fn should_close_connection(request: &HttpRequest<'_>) -> bool {
    let mut connection_header = None;
    for header in request.headers.iter() {
        if header.name.eq_ignore_ascii_case("Connection") {
            connection_header = Some(header.value);
            break;
        }
    }

    let is_http_11 = request.version.eq_ignore_ascii_case("HTTP/1.1");
    if let Some(value) = connection_header {
        if value.eq_ignore_ascii_case("close") {
            return true;
        }
        if value.eq_ignore_ascii_case("keep-alive") {
            return false;
        }
    }

    // HTTP/1.1 defaults to keep-alive; HTTP/1.0 and others default to close.
    !is_http_11
}

/// Type alias for `HttpServer` with default buffer sizes (4KB each)
pub type DefaultHttpServer =
    HttpServer<SERVER_BUFFER_SIZE, SERVER_BUFFER_SIZE, MAX_REQUEST_SIZE, DEFAULT_MAX_RESPONSE_SIZE>;

/// Type alias for `HttpServer` with small buffer sizes for memory-constrained environments (1KB each)
pub type SmallHttpServer = HttpServer<1024, 1024, 1024, 1024>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_server_creation() {
        let server: DefaultHttpServer = HttpServer::new(8080);
        assert_eq!(server.port, 8080);
        assert_eq!(server.timeouts.accept_timeout, 10);
        assert_eq!(server.timeouts.read_timeout, 30);
        assert_eq!(server.timeouts.handler_timeout, 60);

        let server: SmallHttpServer = HttpServer::new(3000);
        assert_eq!(server.port, 3000);
    }

    #[test]
    fn test_server_timeouts() {
        // Test default timeouts
        let timeouts = ServerTimeouts::default();
        assert_eq!(timeouts.accept_timeout, 10);
        assert_eq!(timeouts.read_timeout, 30);
        assert_eq!(timeouts.handler_timeout, 60);

        // Test custom timeouts
        let custom_timeouts = ServerTimeouts::new(5, 15, 45);
        assert_eq!(custom_timeouts.accept_timeout, 5);
        assert_eq!(custom_timeouts.read_timeout, 15);
        assert_eq!(custom_timeouts.handler_timeout, 45);

        // Test server with custom timeouts
        let server = HttpServer::<1024, 1024, 1024, 1024>::with_timeouts(8080, custom_timeouts);
        assert_eq!(server.port, 8080);
        assert_eq!(server.timeouts.accept_timeout, 5);
        assert_eq!(server.timeouts.read_timeout, 15);
        assert_eq!(server.timeouts.handler_timeout, 45);
    }
}

//! A minimal SMTP server for development and testing.
//!
//! Accepts EHLO/HELO, AUTH (accepted), MAIL FROM, RCPT TO, DATA, RSET, NOOP
//! and QUIT, and captures each delivered message verbatim.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

/// A running mock SMTP server that records delivered messages.
pub struct MockSmtpServer {
    pub local_addr: SocketAddr,
    delivered: Arc<Mutex<Vec<Vec<u8>>>>,
    shutdown: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl MockSmtpServer {
    /// Start on `127.0.0.1:port` (0 picks a free port).
    pub async fn start_on(port: u16) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        let local_addr = listener.local_addr()?;
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(accept_loop(listener, delivered.clone(), shutdown_rx));
        Ok(Self {
            local_addr,
            delivered,
            shutdown: Mutex::new(Some(shutdown_tx)),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// A copy of every message body received so far.
    pub async fn delivered(&self) -> Vec<Vec<u8>> {
        self.delivered.lock().await.clone()
    }

    pub async fn shutdown(self) {
        if let Some(sender) = self.shutdown.lock().await.take() {
            sender.send(()).ok();
        }
    }
}

async fn accept_loop(
    listener: TcpListener,
    delivered: Arc<Mutex<Vec<Vec<u8>>>>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((socket, _)) = accepted else { break };
                let delivered = delivered.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(socket, delivered).await {
                        tracing::debug!("mock smtp connection error: {error}");
                    }
                });
            }
            _ = &mut shutdown => break,
        }
    }
}

async fn handle_connection(socket: TcpStream, delivered: Arc<Mutex<Vec<Vec<u8>>>>) -> io::Result<()> {
    socket.set_nodelay(true).ok();
    let mut stream = BufReader::new(socket);
    stream.write_all(b"220 jmail mock SMTP ready\r\n").await?;

    let mut line = String::new();
    loop {
        line.clear();
        if stream.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let command = line.trim_end();
        let upper = command.to_uppercase();

        if upper.starts_with("EHLO") || upper.starts_with("HELO") {
            stream
                .write_all(b"250-jmail mock SMTP\r\n250 AUTH PLAIN LOGIN\r\n")
                .await?;
        } else if upper.starts_with("AUTH LOGIN") {
            stream.write_all(b"334 VXNlcm5hbWU6\r\n").await?;
            read_discard(&mut stream).await?;
            stream.write_all(b"334 UGFzc3dvcmQ6\r\n").await?;
            read_discard(&mut stream).await?;
            stream.write_all(b"235 2.7.0 Authentication successful\r\n").await?;
        } else if upper.starts_with("AUTH PLAIN") {
            // AUTH PLAIN <base64> may be on the same line or a continuation.
            if command.split_whitespace().count() < 3 {
                stream.write_all(b"334 \r\n").await?;
                read_discard(&mut stream).await?;
            }
            stream.write_all(b"235 2.7.0 Authentication successful\r\n").await?;
        } else if upper.starts_with("MAIL FROM") {
            stream.write_all(b"250 2.1.0 Sender OK\r\n").await?;
        } else if upper.starts_with("RCPT TO") {
            stream.write_all(b"250 2.1.5 Recipient OK\r\n").await?;
        } else if upper.starts_with("DATA") {
            stream
                .write_all(b"354 End data with <CR><LF>.<CR><LF>\r\n")
                .await?;
            let data = read_data(&mut stream).await?;
            delivered.lock().await.push(data);
            stream.write_all(b"250 2.0.0 OK queued\r\n").await?;
        } else if upper.starts_with("RSET") || upper.starts_with("NOOP") {
            stream.write_all(b"250 2.0.0 OK\r\n").await?;
        } else if upper.starts_with("QUIT") {
            stream.write_all(b"221 2.0.0 Bye\r\n").await?;
            return Ok(());
        } else {
            stream.write_all(b"250 2.0.0 OK\r\n").await?;
        }
    }
}

async fn read_discard(stream: &mut BufReader<TcpStream>) -> io::Result<()> {
    let mut line = String::new();
    stream.read_line(&mut line).await.map(|_| ())
}

/// Read a DATA block up to the terminating `.` line, undoing dot-stuffing.
async fn read_data(stream: &mut BufReader<TcpStream>) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        if stream.read_line(&mut line).await? == 0 {
            break;
        }
        if line == ".\r\n" || line == ".\n" {
            break;
        }
        let content = line.strip_prefix('.').unwrap_or(&line);
        data.extend_from_slice(content.as_bytes());
    }
    Ok(data)
}

//! A TCP relay between a client and a real endpoint that breaks the next
//! connection whose request matches, the way a network does: a reset before
//! the answer, a reset part-way through the answer, or a stall.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone, Debug)]
pub enum Fault {
    /// Close the connection once the request is forwarded, before any answer.
    ResetBeforeAnswer,
    /// Forward this many bytes of the answer, then close.
    ResetAfter(usize),
    /// Forward this many bytes of the answer, then hold the rest back.
    StallAfter(usize, Duration),
}

struct Armed {
    matching: String,
    fault: Fault,
}

#[derive(Clone, Default)]
pub struct Relay {
    armed: Arc<Mutex<Vec<Armed>>>,
    hits: Arc<Mutex<u32>>,
}

pub struct RelayHandle {
    pub endpoint: String,
    pub relay: Relay,
    _task: tokio::task::JoinHandle<()>,
}

impl Relay {
    pub async fn start(upstream: &str) -> RelayHandle {
        let upstream = upstream
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let relay = Relay::default();
        let serving = relay.clone();
        let task = tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let relay = serving.clone();
                let upstream = upstream.clone();
                tokio::spawn(async move {
                    let _ = relay.pipe(client, &upstream).await;
                });
            }
        });
        RelayHandle {
            endpoint,
            relay,
            _task: task,
        }
    }

    /// The next request whose head contains `matching` meets `fault`.
    pub fn arm(&self, matching: &str, fault: Fault) {
        self.armed.lock().unwrap().push(Armed {
            matching: matching.to_string(),
            fault,
        });
    }

    /// How many armed faults fired.
    pub fn hits(&self) -> u32 {
        *self.hits.lock().unwrap()
    }

    fn take(&self, head: &[u8]) -> Option<Fault> {
        let head = String::from_utf8_lossy(head);
        let mut armed = self.armed.lock().unwrap();
        let at = armed.iter().position(|a| head.contains(&a.matching))?;
        *self.hits.lock().unwrap() += 1;
        Some(armed.remove(at).fault)
    }

    async fn pipe(&self, mut client: TcpStream, upstream: &str) -> std::io::Result<()> {
        let mut server = TcpStream::connect(upstream).await?;
        let (mut client_read, mut client_write) = client.split();
        let (mut server_read, mut server_write) = server.split();
        let mut fault: Option<Fault> = None;
        let mut answered = 0usize;
        let mut up = vec![0u8; 64 * 1024];
        let mut down = vec![0u8; 64 * 1024];
        loop {
            tokio::select! {
                n = client_read.read(&mut up) => {
                    let n = n?;
                    if n == 0 {
                        return Ok(());
                    }
                    if fault.is_none() {
                        fault = self.take(&up[..n]);
                        answered = 0;
                    }
                    server_write.write_all(&up[..n]).await?;
                    if matches!(fault, Some(Fault::ResetBeforeAnswer)) {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        return Ok(());
                    }
                }
                n = server_read.read(&mut down) => {
                    let n = n?;
                    if n == 0 {
                        return Ok(());
                    }
                    match fault {
                        Some(Fault::ResetAfter(limit)) if answered + n > limit => {
                            client_write.write_all(&down[..limit.saturating_sub(answered)]).await?;
                            return Ok(());
                        }
                        Some(Fault::StallAfter(limit, hold)) if answered + n > limit => {
                            let cut = limit.saturating_sub(answered);
                            client_write.write_all(&down[..cut]).await?;
                            tokio::time::sleep(hold).await;
                            client_write.write_all(&down[cut..n]).await?;
                            answered += n;
                            fault = None;
                        }
                        _ => {
                            client_write.write_all(&down[..n]).await?;
                            answered += n;
                        }
                    }
                }
            }
        }
    }
}

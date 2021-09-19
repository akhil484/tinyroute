use std::time::Duration;

use crate::agent::{Agent, Message};
use crate::client::{
    connect, Client, ClientMessage, ClientReceiver, ClientSender, TcpClient,
};
use crate::errors::Result;
use crate::frame::Frame;
use crate::ToAddress;
use log::{error, info};

#[derive(Debug, Clone)]
pub enum Reconnect {
    Constant(Duration),
    Exponential { seconds: u64, max: Option<u64> },
}

#[derive(Debug, Copy, Clone)]
pub enum Retry {
    Never,
    Forever,
    Count(usize),
}

async fn connect_to(
    addr: impl AsRef<str>,
    reconnect: &mut Reconnect,
    heartbeat: &mut Option<Duration>,
    mut retry: Retry,
) -> Option<(ClientSender, ClientReceiver)> {
    let tcp_client = loop {
        match TcpClient::connect(addr.as_ref()).await {
            Ok(c) => break Some(c),
            Err(e) => {
                let sleep_time = match reconnect {
                    Reconnect::Constant(n) => *n,
                    Reconnect::Exponential { seconds, max } => {
                        let secs = match max {
                            Some(max) => seconds.min(max),
                            None => seconds,
                        };
                        let sleep = Duration::from_secs(*secs);
                        *seconds *= 2;
                        *reconnect = Reconnect::Exponential { seconds: *seconds, max: *max };
                        sleep
                    }
                };
                match retry {
                    Retry::Count(0) => break None,
                    Retry::Never => break None,
                    Retry::Count(ref mut n) => *n -= 1,
                    Retry::Forever => {}
                }
                tokio::time::sleep(sleep_time).await;
                info!("retrying...");
            }
        }
    };

    Some(connect(tcp_client?, *heartbeat))
}

// This is a bit silly but I'm pretty tired
enum Action<T> {
    Message(T),
    Reconnect,
    Continue,
}

impl<T> std::fmt::Display for Action<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message(_) => write!(f, "Message"),
            Self::Reconnect => write!(f, "Reconnect"),
            Self::Continue => write!(f, "Continue"),
        }
    }
}

pub struct Bridge<'addr, T: 'static, A: ToAddress> {
    agent: Agent<T, A>,
    addr: &'addr str,
    reconnect: Reconnect,
    heartbeat: Option<Duration>,
    connection: Option<(ClientSender, ClientReceiver)>,
    retry: Retry,
}

impl<'addr, T: Send + 'static, A: ToAddress> Bridge<'addr, T, A> {
    pub fn new(
        agent: Agent<T, A>,
        addr: &'addr str,
        reconnect: Reconnect,
        retry: Retry,
        heartbeat: Option<Duration>,
    ) -> Self {
        Self { agent, addr, reconnect, heartbeat, retry, connection: None }
    }

    async fn reconnect(&mut self) {
        self.connection = connect_to(
            self.addr,
            &mut self.reconnect,
            &mut self.heartbeat,
            self.retry,
        )
        .await;
    }

    pub async fn run(&mut self) -> Option<Message<T, A>> {
        // Rx here is the incoming data from the network connection.
        // This should never do anything but return `None` once the connection is closed.
        // No data should be sent to this guy
        if let None = self.connection {
            self.reconnect().await;
        }

        if self.connection.is_none() {
            return Some(Message::Shutdown);
        }

        let (bridge_output_tx, rx_client_closed) =
            self.connection.as_mut().unwrap();

        let action = tokio::select! {
            is_closed = rx_client_closed.recv() => {
                let is_closed = is_closed.is_none();
                match is_closed {
                    true => Action::Reconnect,
                    false => Action::Continue,
                }
            },
            msg = self.agent.recv() => {
                match msg {
                    Ok(m) => Action::Message(m),
                    Err(e) => {
                        error!("failed to receive message: {:?}", e);
                        Action::Continue
                    }
                }
            }
        };

        eprintln!("--- Action> {}", action);

        let msg = match action {
            Action::Message(m) => {
                eprintln!("> Message");
                m
            }
            Action::Reconnect => {
                eprintln!("> Reconnect...");
                self.reconnect().await;
                // self.connection = Some(connect_to(self.addr, &mut self.reconnect, &mut self.heartbeat).await);
                return None;
            }
            Action::Continue => {
                eprintln!("> Continue");
                return None;
            }
        };

        eprintln!("Trying to send the message...");

        if let Message::RemoteMessage(bytes, sender) = msg {
            eprintln!("{:?}", std::str::from_utf8(&bytes).unwrap());

            // Send framed messages only!
            let framed_message = Frame::frame_message(&bytes);
            let msg = ClientMessage::Payload(framed_message);
            let res = bridge_output_tx.send(msg).await;
            eprintln!("Mesage sent: > {:?}", res.is_ok());
            None
        } else {
            Some(msg)
        }
    }
}
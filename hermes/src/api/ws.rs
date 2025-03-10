use {
    super::types::{
        PriceIdInput,
        RpcPriceFeed,
    },
    crate::store::{
        types::RequestTime,
        Store,
    },
    anyhow::{
        anyhow,
        Result,
    },
    axum::{
        extract::{
            ws::{
                Message,
                WebSocket,
                WebSocketUpgrade,
            },
            State,
        },
        response::IntoResponse,
    },
    dashmap::DashMap,
    futures::{
        future::join_all,
        stream::{
            SplitSink,
            SplitStream,
        },
        SinkExt,
        StreamExt,
    },
    pyth_sdk::PriceIdentifier,
    serde::{
        Deserialize,
        Serialize,
    },
    std::{
        collections::HashMap,
        pin::Pin,
        sync::{
            atomic::{
                AtomicUsize,
                Ordering,
            },
            Arc,
        },
        time::Duration,
    },
    tokio::sync::mpsc,
};

pub const PING_INTERVAL_DURATION: Duration = Duration::from_secs(30);
pub const NOTIFICATIONS_CHAN_LEN: usize = 1000;

pub async fn ws_route_handler(
    ws: WebSocketUpgrade,
    State(state): State<super::State>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| websocket_handler(socket, state))
}

async fn websocket_handler(stream: WebSocket, state: super::State) {
    let ws_state = state.ws.clone();
    let id = ws_state.subscriber_counter.fetch_add(1, Ordering::SeqCst);
    log::debug!("New websocket connection, assigning id: {}", id);

    let (notify_sender, notify_receiver) = mpsc::channel::<()>(NOTIFICATIONS_CHAN_LEN);
    let (sender, receiver) = stream.split();
    let mut subscriber =
        Subscriber::new(id, state.store.clone(), notify_receiver, receiver, sender);

    ws_state.subscribers.insert(id, notify_sender);
    subscriber.run().await;
}

pub type SubscriberId = usize;

/// Subscriber is an actor that handles a single websocket connection.
/// It listens to the store for updates and sends them to the client.
pub struct Subscriber {
    id:                      SubscriberId,
    closed:                  bool,
    store:                   Arc<Store>,
    notify_receiver:         mpsc::Receiver<()>,
    receiver:                SplitStream<WebSocket>,
    sender:                  SplitSink<WebSocket, Message>,
    price_feeds_with_config: HashMap<PriceIdentifier, PriceFeedClientConfig>,
    ping_interval_future:    Pin<Box<tokio::time::Sleep>>,
    responded_to_ping:       bool,
}

impl Subscriber {
    pub fn new(
        id: SubscriberId,
        store: Arc<Store>,
        notify_receiver: mpsc::Receiver<()>,
        receiver: SplitStream<WebSocket>,
        sender: SplitSink<WebSocket, Message>,
    ) -> Self {
        Self {
            id,
            closed: false,
            store,
            notify_receiver,
            receiver,
            sender,
            price_feeds_with_config: HashMap::new(),
            ping_interval_future: Box::pin(tokio::time::sleep(PING_INTERVAL_DURATION)),
            responded_to_ping: true, // We start with true so we don't close the connection immediately
        }
    }

    pub async fn run(&mut self) {
        while !self.closed {
            if let Err(e) = self.handle_next().await {
                log::warn!("Subscriber {}: Error handling next message: {}", self.id, e);
                break;
            }
        }
    }

    async fn handle_next(&mut self) -> Result<()> {
        tokio::select! {
            maybe_update_feeds = self.notify_receiver.recv() => {
                if maybe_update_feeds.is_none() {
                    return Err(anyhow!("Update channel closed. This should never happen. Closing connection."));
                };
                self.handle_price_feeds_update().await?;
            },
            maybe_message_or_err = self.receiver.next() => {
                match maybe_message_or_err {
                    None => {
                        log::debug!("Subscriber {} closed connection unexpectedly.", self.id);
                        self.closed = true;
                        return Ok(());
                    },
                    Some(message_or_err) => self.handle_client_message(message_or_err?).await?
                }
            },
            _  = &mut self.ping_interval_future => {
                if !self.responded_to_ping {
                    log::debug!("Subscriber {} did not respond to ping. Closing connection.", self.id);
                    self.closed = true;
                    return Ok(());
                }
                self.responded_to_ping = false;
                self.sender.send(Message::Ping(vec![])).await?;
                self.ping_interval_future = Box::pin(tokio::time::sleep(PING_INTERVAL_DURATION));
            }
        }

        Ok(())
    }

    async fn handle_price_feeds_update(&mut self) -> Result<()> {
        let price_feed_ids = self.price_feeds_with_config.keys().cloned().collect();
        for update in self
            .store
            .get_price_feeds_with_update_data(price_feed_ids, RequestTime::Latest)
            .await?
            .price_feeds
        {
            let config = self
                .price_feeds_with_config
                .get(&PriceIdentifier::new(update.price_feed.feed_id))
                .ok_or(anyhow::anyhow!(
                    "Config missing, price feed list was poisoned during iteration."
                ))?;

            self.sender
                .feed(Message::Text(serde_json::to_string(
                    &ServerMessage::PriceUpdate {
                        price_feed: RpcPriceFeed::from_price_feed_update(
                            update,
                            config.verbose,
                            config.binary,
                        ),
                    },
                )?))
                .await?;
        }

        self.sender.flush().await?;
        Ok(())
    }

    async fn handle_client_message(&mut self, message: Message) -> Result<()> {
        if let Message::Close(_) = message {
            log::debug!("Subscriber {} closed connection", self.id);
            self.closed = true;
            return Ok(());
        }

        let maybe_client_message = match message {
            Message::Text(text) => serde_json::from_str::<ClientMessage>(&text),
            Message::Binary(data) => serde_json::from_slice::<ClientMessage>(&data),
            Message::Ping(_) => {
                // Axum will send Pong automatically
                return Ok(());
            }
            Message::Pong(_) => {
                self.responded_to_ping = true;
                return Ok(());
            }
            _ => {
                return Ok(());
            }
        };

        match maybe_client_message {
            Err(e) => {
                self.sender
                    .send(
                        serde_json::to_string(&ServerMessage::Response(
                            ServerResponseMessage::Err {
                                error: e.to_string(),
                            },
                        ))?
                        .into(),
                    )
                    .await?;
                return Ok(());
            }
            Ok(ClientMessage::Subscribe {
                ids,
                verbose,
                binary,
            }) => {
                for id in ids {
                    let price_id: PriceIdentifier = id.into();
                    self.price_feeds_with_config
                        .insert(price_id, PriceFeedClientConfig { verbose, binary });
                }
            }
            Ok(ClientMessage::Unsubscribe { ids }) => {
                for id in ids {
                    let price_id: PriceIdentifier = id.into();
                    self.price_feeds_with_config.remove(&price_id);
                }
            }
        }

        self.sender
            .send(
                serde_json::to_string(&ServerMessage::Response(ServerResponseMessage::Ok))?.into(),
            )
            .await?;

        Ok(())
    }
}

pub async fn notify_updates(ws_state: Arc<WsState>) {
    let closed_subscribers: Vec<Option<SubscriberId>> = join_all(
        ws_state
            .subscribers
            .iter_mut()
            .map(|subscriber| async move {
                match subscriber.send(()).await {
                    Ok(_) => None,
                    Err(e) => {
                        log::debug!("Error sending update to subscriber: {}", e);
                        Some(*subscriber.key())
                    }
                }
            }),
    )
    .await;

    // Remove closed_subscribers from ws_state
    closed_subscribers.into_iter().for_each(|id| {
        if let Some(id) = id {
            ws_state.subscribers.remove(&id);
        }
    });
}

#[derive(Clone)]
pub struct PriceFeedClientConfig {
    verbose: bool,
    binary:  bool,
}

pub struct WsState {
    pub subscriber_counter: AtomicUsize,
    pub subscribers:        DashMap<SubscriberId, mpsc::Sender<()>>,
}

impl WsState {
    pub fn new() -> Self {
        Self {
            subscriber_counter: AtomicUsize::new(0),
            subscribers:        DashMap::new(),
        }
    }
}


#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "type")]
enum ClientMessage {
    #[serde(rename = "subscribe")]
    Subscribe {
        ids:     Vec<PriceIdInput>,
        #[serde(default)]
        verbose: bool,
        #[serde(default)]
        binary:  bool,
    },
    #[serde(rename = "unsubscribe")]
    Unsubscribe { ids: Vec<PriceIdInput> },
}


#[derive(Serialize, Debug, Clone)]
#[serde(tag = "type")]
enum ServerMessage {
    #[serde(rename = "response")]
    Response(ServerResponseMessage),
    #[serde(rename = "price_update")]
    PriceUpdate { price_feed: RpcPriceFeed },
}

#[derive(Serialize, Debug, Clone)]
#[serde(tag = "status")]
enum ServerResponseMessage {
    #[serde(rename = "ok")]
    Ok,
    #[serde(rename = "error")]
    Err { error: String },
}

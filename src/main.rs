//! Example websocket server.
//!
//! Run with
//!
//! ```not_rust
//! cargo run -p example-websockets
//! ```
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Extension, Path, TypedHeader,
    },
    response::IntoResponse,
    routing::get,
    AddExtensionLayer, Router,
};
use futures::{sink::SinkExt, stream::StreamExt};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::sync::{
    broadcast::{self, Sender},
    mpsc::{self, Receiver},
    Mutex,
};

use tower_http::trace::{DefaultMakeSpan, TraceLayer};

pub struct ClipboardSession {
    pub id: String,
    pub next_conn_id: u32,
    pub members: Vec<u32>,
    pub member_socket_txs: HashMap<u32, mpsc::Sender<ClipboardMessage>>,
    pub broadcast_tx: Sender<ClipboardMessage>,
}

impl ClipboardSession {
    pub fn new(sess: &str) -> Self {
        let (tx, _) = broadcast::channel(256);

        ClipboardSession {
            id: sess.to_string(),
            next_conn_id: 1,
            members: vec![],
            broadcast_tx: tx,
            member_socket_txs: HashMap::new(),
        }
    }

    pub fn add_member(&mut self) -> (u32, Receiver<ClipboardMessage>) {
        let id = self.next_conn_id;
        self.next_conn_id += 1;

        self.members.push(id);
        let (socket_tx, socket_rx) = mpsc::channel(256);
        self.member_socket_txs.insert(id, socket_tx);
        (id, socket_rx)
    }

    pub fn list_members(&self) -> Vec<ClipboardSessionMember> {
        self.members
            .iter()
            .map(|conn_id| ClipboardSessionMember {
                conn_id: conn_id.to_owned(),
            })
            .collect()
    }

    pub fn remove_member(&mut self, conn_id: u32) {
        let index = self.members.iter().position(|x| *x == conn_id).unwrap();
        self.members.remove(index);
    }
}

struct ClipboardSessionManager {
    pub sessions: HashMap<String, Arc<Mutex<ClipboardSession>>>,
}

impl ClipboardSessionManager {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }

    pub async fn join_session(
        &mut self,
        session_id: &str,
    ) -> (
        u32,
        Receiver<ClipboardMessage>,
        Arc<Mutex<ClipboardSession>>,
    ) {
        let sess = {
            match self.sessions.get_mut(session_id) {
                Some(sess) => sess,
                None => {
                    let ses = ClipboardSession::new(session_id);
                    self.sessions
                        .insert(session_id.to_string(), Arc::new(Mutex::new(ses)));
                    self.sessions.get(session_id).unwrap()
                }
            }
        };
        let (uid, socket_rx) = sess.lock().await.add_member();

        (uid, socket_rx, sess.to_owned())
    }
}

#[derive(Clone)]
struct AppState {
    clipboard_sessions: Arc<Mutex<ClipboardSessionManager>>,
}

#[tokio::main]
async fn main() {
    // Set the RUST_LOG, if it hasn't been explicitly defined
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "example_websockets=debug,tower_http=debug")
    }
    tracing_subscriber::fmt::init();

    let clipboard_sessions = Arc::new(Mutex::new(ClipboardSessionManager::new()));

    // build our application with some routes
    let app = Router::new()
        // routes are matched from bottom to top, so we have to put `nest` at the
        // top since it matches all routes
        .route("/ws/clipboard/:session_id", get(ws_handler))
        .layer(AddExtensionLayer::new(AppState { clipboard_sessions }))
        // logging so we can see whats going on
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        );

    // let msg = ClipboardMessage::IceCandidate(IceCandidateData { conn_id: 1, ice_candidate: "sssss".to_string()});
    // println!("{:?}", serde_json::to_string(&msg));

    // run it with hyper
    let addr = SocketAddr::from(([0, 0, 0, 0], 3003));
    tracing::debug!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Extension(state): Extension<AppState>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    // if let Some(TypedHeader(user_agent)) = user_agent {
    //     println!("`{}` connected", user_agent.as_str());
    // }

    let mut g = state.clipboard_sessions.lock().await;
    let (conn_id, socket_rx, sess) = g.join_session(&session_id).await;

    ws.on_upgrade(move |socket| handle_socket(socket, conn_id, socket_rx, sess))
}

async fn handle_socket(
    socket: WebSocket,
    conn_id: u32,
    mut socket_rx: mpsc::Receiver<ClipboardMessage>,
    session: Arc<Mutex<ClipboardSession>>,
) {
    let tx = { session.lock().await.broadcast_tx.clone() };
    let (mut sender, mut receiver) = socket.split();

    tokio::spawn(async move {
        while let Some(res) = socket_rx.recv().await {
            if sender.send(res.to_msg()).await.is_err() {
                break;
            }
        }
    });

    let mut rx = tx.subscribe();

    // 广播成员
    let members = {
        session
            .lock()
            .await
            .list_members()
            .into_iter()
            .filter(|m| m.conn_id != conn_id)
            .collect()
    };

    let socket_tx = {
        session
            .lock()
            .await
            .member_socket_txs
            .get(&conn_id)
            .unwrap()
            .clone()
    };

    socket_tx
        .send(ClipboardMessage::Members(members))
        .await
        .unwrap();

    tokio::spawn(async move {
        while let Ok(res) = rx.recv().await {
            // println!("Broadcast: {:?}", res);

            socket_tx.send(res).await.unwrap();
        }
    });

    loop {
        if let Some(Ok(msg)) = receiver.next().await {
            // 1. 心跳包
            // 2. ice candidates

            match msg {
                Message::Text(txt) => {
                    if let Ok(res) = serde_json::from_str::<ClipboardMessage>(&txt) {
                        match res {
                            ClipboardMessage::IceCandidate(mut candidate) => {
                                candidate.conn_id = conn_id; // 输入的 conn_id 无用
                                tx.send(ClipboardMessage::IceCandidate(candidate));
                            }

                            ClipboardMessage::Offer(mut data) => {
                                // 当前conn offer 目标conn
                                if let Some(target_socket_tx) =
                                    session.lock().await.member_socket_txs.get(&data.conn_id)
                                {
                                    data.conn_id = conn_id; // 对接收方来说， conn_id 设置为发送方
                                    target_socket_tx.send(ClipboardMessage::Offer(data)).await;
                                }
                            }

                            ClipboardMessage::Answer(mut data) => {
                                // 当前conn answer 目标conn
                                if let Some(target_socket_tx) =
                                    session.lock().await.member_socket_txs.get(&data.conn_id)
                                {
                                    data.conn_id = conn_id; // 对接收方来说， conn_id 设置为发送方
                                    target_socket_tx.send(ClipboardMessage::Answer(data)).await;
                                }
                            }

                            _default => {}
                        }
                    } else {
                        println!("Invalid Message {:?}", txt);
                    }
                }
                Message::Ping(_) => panic!("not support Message::Ping"),
                Message::Pong(_) => panic!("not support Message::Pong"),
                Message::Binary(_) => panic!("not support Message::Binary"),
                Message::Close(_) => {}
            }
        } else {
            // 丢失链接, session内广播
            break;
        }
    }

    session.lock().await.remove_member(conn_id);
    tx.send(ClipboardMessage::Lost(ClipboardSessionMember { conn_id }));
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ClipboardSessionMember {
    pub conn_id: u32,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OfferData {
    pub conn_id: u32,
    pub offer: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AnswerData {
    pub conn_id: u32,
    pub answer: String,
}
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct IceCandidateData {
    pub conn_id: u32,
    pub ice_candidate: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type", content = "payload", rename_all = "camelCase")]
pub enum ClipboardMessage {
    // 发送所有成员
    Members(Vec<ClipboardSessionMember>),

    // 新增
    IceCandidate(IceCandidateData),

    // Offer
    Offer(OfferData),

    // Answer
    Answer(AnswerData),

    // Lost Member
    Lost(ClipboardSessionMember),
}

impl ClipboardMessage {
    pub fn to_msg(&self) -> Message {
        Message::Text(serde_json::to_string(self).unwrap())
    }
}

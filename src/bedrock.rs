use std::io::{Cursor, Read};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::{BufMut, BytesMut};
use crossbeam::atomic::AtomicCell;
use pumpkin_protocol::bedrock::packet_decoder::BedrockBatchDecoder;
use pumpkin_protocol::bedrock::packet_encoder::BedrockBatchEncoder;
use pumpkin_protocol::bedrock::server::animate::{AnimateAction, SAnimate};
use pumpkin_protocol::bedrock::server::text::SText;
use pumpkin_protocol::bedrock::{BEDROCK_GAME_PACKET, SubClient};
use pumpkin_protocol::codec::var_int::VarInt;
use pumpkin_protocol::codec::var_long::VarLong;
use pumpkin_protocol::codec::var_uint::VarUInt;
use pumpkin_protocol::codec::var_ulong::VarULong;
use pumpkin_protocol::serial::PacketWrite;
use rand::RngExt;
use serde_json::json;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Notify, mpsc};
use webrtc::data_channel::{DataChannel, DataChannelEvent, RTCDataChannelInit};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceGatheringState, RTCPeerConnectionState, RTCSessionDescription, SettingEngineBuilder,
};

use crate::Args;

pub const BEDROCK_PROTOCOL_VERSION: i32 = 2193;
pub const BEDROCK_GAME_VERSION: &str = "1.26.51";
const MAX_FRAGMENT_SIZE: usize = 10_000;

// Valid SEC1 uncompressed public key point on NIST P-384 generator G (97 bytes) encoded as Base64
const P384_BASE_POINT_B64: &str = "BKqHyiK+iwU3jrHHHvMgrXRuHTtii6ebmFn3QeCCVCo4VQLyXb9VKWw6VF44cnYKtzYX3kqWJixvXZ6Yv5KS3Cn49B29KJoUfOnaMRO18LjACmCxzh1+gZ16Qx18kOoOXw==";

struct ClientPeerHandler {
    gathering_notify: Arc<Notify>,
    closed_notify: Arc<Notify>,
}

#[async_trait]
impl PeerConnectionEventHandler for ClientPeerHandler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gathering_notify.notify_waiters();
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if matches!(
            state,
            RTCPeerConnectionState::Failed
                | RTCPeerConnectionState::Disconnected
                | RTCPeerConnectionState::Closed
        ) {
            self.closed_notify.notify_waiters();
        }
    }
}

fn remove_component_two_candidates(sdp: &str) -> String {
    let mut filtered = String::with_capacity(sdp.len());
    for line in sdp.lines().filter(|line| {
        line.strip_prefix("a=candidate:")
            .and_then(|candidate| candidate.split_whitespace().nth(1))
            != Some("2")
    }) {
        filtered.push_str(line);
        filtered.push_str("\r\n");
    }
    filtered
}

/// Simple HTTP POST helper to submit client SDP offer and retrieve server SDP answer
pub async fn http_post_sdp(
    addr: SocketAddr,
    sdp_offer: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut stream = TcpStream::connect(addr).await?;
    let request = format!(
        "POST /v1/join/0 HTTP/1.1\r\n\
         Host: {}\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        addr,
        sdp_offer.len(),
        sdp_offer
    );
    stream.write_all(request.as_bytes()).await?;

    let mut response_bytes = Vec::new();
    stream.read_to_end(&mut response_bytes).await?;

    let response_str = String::from_utf8(response_bytes)?;
    let mut header_body_parts = response_str.splitn(2, "\r\n\r\n");
    let headers = header_body_parts
        .next()
        .ok_or("Empty response from NetherNet signaling server")?;
    let body = header_body_parts
        .next()
        .ok_or("Missing response body from NetherNet signaling server")?;

    if !headers.starts_with("HTTP/1.1 200") && !headers.starts_with("HTTP/1.0 200") {
        return Err(format!(
            "NetherNet signaling server returned error: {}",
            headers.lines().next().unwrap_or("")
        )
        .into());
    }

    Ok(body.to_string())
}

#[derive(Default)]
struct FragmentBuffer {
    next_remaining: Option<u8>,
    data: Vec<u8>,
}

impl FragmentBuffer {
    fn push(&mut self, remaining: u8, payload: &[u8]) -> Option<Vec<u8>> {
        match self.next_remaining {
            None if remaining > 0 => {
                self.next_remaining = Some(remaining - 1);
                self.data.clear();
                self.data.extend_from_slice(payload);
                None
            }
            None => Some(payload.to_vec()),
            Some(expected) if expected == remaining => {
                self.next_remaining = remaining.checked_sub(1);
                self.data.extend_from_slice(payload);
                if remaining == 0 {
                    self.next_remaining = None;
                    Some(std::mem::take(&mut self.data))
                } else {
                    None
                }
            }
            Some(_) => {
                self.next_remaining = None;
                self.data.clear();
                None
            }
        }
    }
}

pub struct BedrockClient {
    pub username: String,
    pub server_addr: SocketAddr,
    pub closed: Arc<AtomicBool>,
    pub is_loaded: Arc<AtomicBool>,

    reliable_channel: Arc<dyn DataChannel>,
    unreliable_channel: Arc<dyn DataChannel>,
    _peer: Arc<dyn PeerConnection>,

    encoder: Mutex<BedrockBatchEncoder>,
    decoder: Mutex<BedrockBatchDecoder>,

    incoming_rx: Mutex<mpsc::Receiver<Vec<u8>>>,

    runtime_entity_id: AtomicU64,
    tick_counter: AtomicU64,

    // Movement & Physics
    pub current_x: AtomicCell<f64>,
    pub current_y: AtomicCell<f64>,
    pub current_z: AtomicCell<f64>,
    pub ground_y: AtomicCell<f64>,
    pub on_ground: AtomicBool,

    pub velocity_x: AtomicCell<f64>,
    pub velocity_y: AtomicCell<f64>,
    pub velocity_z: AtomicCell<f64>,

    last_sent_x: AtomicCell<f64>,
    last_sent_y: AtomicCell<f64>,
    last_sent_z: AtomicCell<f64>,
    last_sent_yaw: AtomicCell<f32>,
    last_sent_pitch: AtomicCell<f32>,

    // Movement AI
    is_walking: AtomicBool,
    move_ticks: AtomicU32,
    move_cooldown: AtomicU32,
    move_dir_x: AtomicCell<f64>,
    move_dir_z: AtomicCell<f64>,
    jump_cooldown: AtomicU32,

    swing_cooldown: AtomicU32,
    message_spam_cooldown: AtomicU32,
    message_count: AtomicU32,
}

impl BedrockClient {
    pub async fn connect(
        server_addr: SocketAddr,
        username: String,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error + Send + Sync>> {
        let config = RTCConfigurationBuilder::default().build();
        let setting_engine = SettingEngineBuilder::default().build();
        let ice_bind_addr = SocketAddr::from(([0, 0, 0, 0], 0));
        let gathering_notify = Arc::new(Notify::new());
        let closed_notify = Arc::new(Notify::new());
        let handler = Arc::new(ClientPeerHandler {
            gathering_notify: gathering_notify.clone(),
            closed_notify: closed_notify.clone(),
        });

        let peer = Arc::new(
            PeerConnectionBuilder::new()
                .with_configuration(config)
                .with_setting_engine(setting_engine)
                .with_handler(handler)
                .with_udp_addrs(vec![ice_bind_addr])
                .build()
                .await?,
        );

        let reliable_channel = peer
            .create_data_channel("ReliableDataChannel", None)
            .await?;
        let unreliable_init = RTCDataChannelInit {
            ordered: false,
            max_retransmits: Some(0),
            ..Default::default()
        };
        let unreliable_channel = peer
            .create_data_channel("UnreliableDataChannel", Some(unreliable_init))
            .await?;

        let offer = peer.create_offer(None).await?;
        peer.set_local_description(offer).await?;

        // Wait for ICE gathering
        let _ = tokio::time::timeout(Duration::from_millis(500), gathering_notify.notified()).await;
        let local_desc = peer
            .local_description()
            .await
            .ok_or("Failed to get local SDP description")?;
        let sdp_offer = remove_component_two_candidates(&local_desc.sdp);

        // Perform HTTP signaling exchange
        let sdp_answer = http_post_sdp(server_addr, &sdp_offer).await?;
        let remote_desc = RTCSessionDescription::answer(sdp_answer)?;
        peer.set_remote_description(remote_desc).await?;

        let (incoming_tx, incoming_rx) = mpsc::channel(1024);
        let closed = Arc::new(AtomicBool::new(false));
        let is_loaded = Arc::new(AtomicBool::new(false));

        // Listen for data channel messages
        let reliable_open = Arc::new(Notify::new());
        let unreliable_open = Arc::new(Notify::new());

        // Spawn Reliable channel listener
        {
            let rel = reliable_channel.clone();
            let tx = incoming_tx.clone();
            let open_notify = reliable_open.clone();
            let closed_flag = closed.clone();
            tokio::spawn(async move {
                let mut fragments = FragmentBuffer::default();
                while let Some(event) = rel.poll().await {
                    match event {
                        DataChannelEvent::OnOpen => {
                            open_notify.notify_waiters();
                        }
                        DataChannelEvent::OnMessage(msg) => {
                            if let Some((&remaining, payload)) = msg.data.split_first()
                                && let Some(mut packet) = fragments.push(remaining, payload)
                            {
                                // NetherNet delivers batch without 0xfe header, prepend it for decoder
                                let mut full = Vec::with_capacity(packet.len() + 1);
                                full.push(BEDROCK_GAME_PACKET);
                                full.append(&mut packet);
                                let _ = tx.send(full).await;
                            }
                        }
                        DataChannelEvent::OnClose => break,
                        DataChannelEvent::OnError => break,
                        _ => {}
                    }
                }
                closed_flag.store(true, Ordering::Relaxed);
            });
        }

        // Spawn Unreliable channel listener
        {
            let unrel = unreliable_channel.clone();
            let tx = incoming_tx;
            let open_notify = unreliable_open.clone();
            let closed_flag = closed.clone();
            tokio::spawn(async move {
                while let Some(event) = unrel.poll().await {
                    match event {
                        DataChannelEvent::OnOpen => {
                            open_notify.notify_waiters();
                        }
                        DataChannelEvent::OnMessage(msg) => {
                            if let Some((_, payload)) = msg.data.split_first() {
                                let mut full = Vec::with_capacity(payload.len() + 1);
                                full.push(BEDROCK_GAME_PACKET);
                                full.extend_from_slice(payload);
                                let _ = tx.send(full).await;
                            }
                        }
                        DataChannelEvent::OnClose => break,
                        DataChannelEvent::OnError => break,
                        _ => {}
                    }
                }
                closed_flag.store(true, Ordering::Relaxed);
            });
        }

        // Wait for both data channels to open
        let open_wait = async {
            tokio::join!(reliable_open.notified(), unreliable_open.notified());
        };
        tokio::time::timeout(Duration::from_secs(5), open_wait)
            .await
            .map_err(|_| "Timed out waiting for NetherNet data channels to open")?;

        let client = Arc::new(Self {
            username,
            server_addr,
            closed,
            is_loaded,
            reliable_channel,
            unreliable_channel,
            _peer: peer,
            encoder: Mutex::new(BedrockBatchEncoder::new()),
            decoder: Mutex::new(BedrockBatchDecoder::new()),
            incoming_rx: Mutex::new(incoming_rx),
            runtime_entity_id: AtomicU64::new(0),
            tick_counter: AtomicU64::new(0),
            current_x: AtomicCell::new(0.0),
            current_y: AtomicCell::new(0.0),
            current_z: AtomicCell::new(0.0),
            ground_y: AtomicCell::new(0.0),
            on_ground: AtomicBool::new(true),
            velocity_x: AtomicCell::new(0.0),
            velocity_y: AtomicCell::new(0.0),
            velocity_z: AtomicCell::new(0.0),
            last_sent_x: AtomicCell::new(0.0),
            last_sent_y: AtomicCell::new(0.0),
            last_sent_z: AtomicCell::new(0.0),
            last_sent_yaw: AtomicCell::new(0.0),
            last_sent_pitch: AtomicCell::new(0.0),
            is_walking: AtomicBool::new(false),
            move_ticks: AtomicU32::new(0),
            move_cooldown: AtomicU32::new(0),
            move_dir_x: AtomicCell::new(0.0),
            move_dir_z: AtomicCell::new(0.0),
            jump_cooldown: AtomicU32::new(0),
            swing_cooldown: AtomicU32::new(0),
            message_spam_cooldown: AtomicU32::new(10),
            message_count: AtomicU32::new(0),
        });

        // Send initial SRequestNetworkSettings
        client.send_request_network_settings().await?;

        Ok(client)
    }

    async fn send_reliable(
        &self,
        data: &[u8],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.closed.load(Ordering::Relaxed) {
            return Err("Connection closed".into());
        }
        let segment_count = data.len().div_ceil(MAX_FRAGMENT_SIZE).max(1);
        if segment_count > 256 {
            return Err("Packet too large for NetherNet fragments".into());
        }
        for (index, chunk) in data.chunks(MAX_FRAGMENT_SIZE).enumerate() {
            let mut segment = BytesMut::with_capacity(chunk.len() + 1);
            segment.put_u8((segment_count - index - 1) as u8);
            segment.extend_from_slice(chunk);
            self.reliable_channel
                .send(segment)
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    async fn send_unreliable(
        &self,
        data: &[u8],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.closed.load(Ordering::Relaxed) {
            return Err("Connection closed".into());
        }
        if data.len() > MAX_FRAGMENT_SIZE {
            return Err("Unreliable packet too large".into());
        }
        let mut segment = BytesMut::with_capacity(data.len() + 1);
        segment.put_u8(0);
        segment.extend_from_slice(data);
        self.unreliable_channel
            .send(segment)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn send_game_packet(
        &self,
        packet_id: u16,
        payload: &[u8],
        reliable: bool,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut buffer = Vec::new();
        {
            let encoder = self.encoder.lock().await;
            encoder.write_game_packet(
                packet_id,
                SubClient::Main,
                SubClient::Main,
                payload,
                &mut buffer,
            )?;
        }
        // NetherNet transports omit leading 0xfe
        let wire_data = buffer
            .strip_prefix(&[BEDROCK_GAME_PACKET])
            .unwrap_or(&buffer);
        if reliable {
            self.send_reliable(wire_data).await
        } else {
            self.send_unreliable(wire_data).await
        }
    }

    async fn send_request_network_settings(
        &self,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let payload = BEDROCK_PROTOCOL_VERSION.to_be_bytes();
        self.send_game_packet(193, &payload, true).await
    }

    async fn send_login(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let jwt_claim = json!({
            "xname": self.username,
            "xid": "12345678",
            "cpk": P384_BASE_POINT_B64,
        });
        let jwt_claim_str = serde_json::to_string(&jwt_claim)?;
        let jwt_b64 = URL_SAFE_NO_PAD.encode(jwt_claim_str.as_bytes());
        let token_jwt = format!("eyJhbGciOiJub25lIn0.{}.", jwt_b64);

        let auth_payload = json!({
            "AuthenticationType": 2, // SelfSigned
            "Token": token_jwt,
        });
        let auth_bytes = serde_json::to_vec(&auth_payload)?;

        let client_data = json!({
            "ClientRandomId": 123456789i64,
            "DeviceOS": 1, // Android / generic
            "DeviceId": "botmark-client",
            "GameVersion": BEDROCK_GAME_VERSION,
            "LanguageCode": "en_US",
            "CurrentInputMode": 1,
            "DefaultInputMode": 1,
            "UIProfile": 0,
            "ServerAddress": self.server_addr.to_string(),
        });
        let client_data_str = serde_json::to_string(&client_data)?;
        let client_data_b64 = URL_SAFE_NO_PAD.encode(client_data_str.as_bytes());
        let raw_token = format!("eyJhbGciOiJub25lIn0.{}.", client_data_b64);
        let raw_token_bytes = raw_token.as_bytes();

        let connection_request_len = 4 + auth_bytes.len() + 4 + raw_token_bytes.len();
        let mut payload = Vec::new();
        payload.extend_from_slice(&BEDROCK_PROTOCOL_VERSION.to_be_bytes());
        VarUInt(connection_request_len as u32).encode(&mut payload)?;
        payload.extend_from_slice(&(auth_bytes.len() as u32).to_le_bytes());
        payload.extend_from_slice(&auth_bytes);
        payload.extend_from_slice(&(raw_token_bytes.len() as u32).to_le_bytes());
        payload.extend_from_slice(raw_token_bytes);

        self.send_game_packet(1, &payload, true).await
    }

    async fn send_resource_pack_response(
        &self,
        status: u8,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut payload = Vec::new();
        // SResourcePackClientResponse expects status - 1 encoded as VarUInt
        let encoded_status = status.saturating_sub(1);
        VarUInt(encoded_status as u32).encode(&mut payload)?;
        // Empty status name string
        VarUInt(0).encode(&mut payload)?;
        self.send_game_packet(8, &payload, true).await
    }

    async fn send_chunk_radius_and_init(
        &self,
        runtime_id: u64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // SRequestChunkRadius (69): VarInt(8), 8u8
        let mut chunk_payload = Vec::new();
        VarInt(8).encode(&mut chunk_payload)?;
        chunk_payload.push(8);
        self.send_game_packet(69, &chunk_payload, true).await?;

        // SSetLocalPlayerAsInitialized (113): VarULong(runtime_id)
        let mut init_payload = Vec::new();
        VarULong(runtime_id).encode(&mut init_payload)?;
        self.send_game_packet(113, &init_payload, true).await?;

        Ok(())
    }

    pub async fn process_packets(&self) -> bool {
        let packet = {
            let mut rx = self.incoming_rx.lock().await;
            rx.recv().await
        };
        let Some(full_packet) = packet else {
            return false;
        };

        let decompressed = {
            let mut decoder = self.decoder.lock().await;
            match decoder.get_packet_payload(full_packet).await {
                Ok(data) => data,
                Err(e) => {
                    log::warn!("Bedrock packet decompression error: {e}");
                    return true;
                }
            }
        };

        let mut cursor = Cursor::new(decompressed);
        loop {
            if cursor.position() >= cursor.get_ref().len() as u64 {
                break;
            }
            let raw_packet = {
                let mut decoder = self.decoder.lock().await;
                match decoder.get_game_packet(&mut cursor) {
                    Ok(raw) => raw,
                    Err(e) => {
                        log::debug!("Bedrock decode batch packet done or error: {e}");
                        break;
                    }
                }
            };

            if let Err(e) = self.handle_packet(raw_packet.id, &raw_packet.payload).await {
                log::error!("Error handling Bedrock packet ID {}: {}", raw_packet.id, e);
            }
        }

        !self.closed.load(Ordering::Relaxed)
    }

    async fn handle_packet(
        &self,
        id: i32,
        payload: &[u8],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match id {
            // CNetworkSettings (143)
            143 => {
                if payload.len() >= 4 {
                    let threshold = u16::from_le_bytes([payload[0], payload[1]]);
                    let algorithm = u16::from_le_bytes([payload[2], payload[3]]);
                    if algorithm == 0 && threshold > 0 {
                        let mut enc = self.encoder.lock().await;
                        enc.set_compression((threshold as usize, 6));
                        let mut dec = self.decoder.lock().await;
                        dec.set_compression(threshold as usize);
                        log::debug!("Bedrock compression enabled: threshold={threshold}");
                    }
                }
                self.send_login().await?;
            }
            // CPlayStatus (2)
            2 => {
                if payload.len() >= 4 {
                    let status =
                        i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    match status {
                        0 => log::info!("Bedrock bot {}: LoginSuccess", self.username),
                        1 => {
                            log::error!("Bedrock bot {}: OutdatedClient", self.username);
                            self.closed.store(true, Ordering::Relaxed);
                        }
                        2 => {
                            log::error!("Bedrock bot {}: OutdatedServer", self.username);
                            self.closed.store(true, Ordering::Relaxed);
                        }
                        3 => log::debug!("Bedrock bot {}: PlayerSpawn", self.username),
                        _ => log::debug!("Bedrock bot {}: PlayStatus {}", self.username, status),
                    }
                }
            }
            // CResourcePacksInfo (6)
            6 => {
                // STATUS_HAVE_ALL_PACKS = 3
                self.send_resource_pack_response(3).await?;
            }
            // CResourcePackStackPacket (7)
            7 => {
                // STATUS_COMPLETED = 4
                self.send_resource_pack_response(4).await?;
            }
            // CStartGame (11)
            11 => {
                let mut c = Cursor::new(payload);
                let _entity_id = VarLong::decode(&mut c)?;
                let runtime_id = VarULong::decode(&mut c)?.0;
                let _gamemode = VarInt::decode(&mut c)?;

                let mut float_buf = [0u8; 4];
                c.read_exact(&mut float_buf)?;
                let x = f32::from_le_bytes(float_buf) as f64;
                c.read_exact(&mut float_buf)?;
                let y = f32::from_le_bytes(float_buf) as f64;
                c.read_exact(&mut float_buf)?;
                let z = f32::from_le_bytes(float_buf) as f64;
                c.read_exact(&mut float_buf)?;
                let pitch = f32::from_le_bytes(float_buf);
                c.read_exact(&mut float_buf)?;
                let yaw = f32::from_le_bytes(float_buf);

                self.runtime_entity_id.store(runtime_id, Ordering::Relaxed);
                self.current_x.store(x);
                self.current_y.store(y);
                self.current_z.store(z);
                self.ground_y.store(y);
                self.last_sent_x.store(x);
                self.last_sent_y.store(y);
                self.last_sent_z.store(z);
                self.last_sent_yaw.store(yaw);
                self.last_sent_pitch.store(pitch);

                self.send_chunk_radius_and_init(runtime_id).await?;
                self.is_loaded.store(true, Ordering::Relaxed);
                log::info!(
                    "Bedrock bot {} spawned into world at ({:.2}, {:.2}, {:.2}) with runtime ID {}",
                    self.username,
                    x,
                    y,
                    z,
                    runtime_id
                );
            }
            // Other packets
            _ => {}
        }
        Ok(())
    }

    pub async fn tick(&self, args: &Args) {
        if !self.is_loaded.load(Ordering::Relaxed) || self.closed.load(Ordering::Relaxed) {
            return;
        }

        let _tick = self.tick_counter.fetch_add(1, Ordering::Relaxed);

        if args.enable_physics {
            self.tick_physics(args);
        }

        // Send SPlayerAuthInput every tick over UnreliableDataChannel
        if let Err(e) = self.send_auth_input().await {
            log::debug!("Error sending SPlayerAuthInput: {e}");
        }

        // Arm swing
        if args.enable_swing {
            let current = self.swing_cooldown.load(Ordering::Relaxed);
            if current > 0 {
                self.swing_cooldown.store(current - 1, Ordering::Relaxed);
            } else {
                let next_cooldown = {
                    let mut rng = rand::rng();
                    rng.random_range(15..40)
                };
                self.swing_cooldown.store(next_cooldown, Ordering::Relaxed);

                let runtime_id = self.runtime_entity_id.load(Ordering::Relaxed);
                let mut payload = Vec::new();
                let swing = SAnimate {
                    action: AnimateAction::SwingArm,
                    target_actor_runtime_id: VarULong(runtime_id),
                    data: 0.0,
                    swing_source: None,
                };
                if PacketWrite::write(&swing, &mut payload).is_ok() {
                    let _ = self.send_game_packet(44, &payload, true).await;
                }
            }
        }

        // Chat spam
        if args.enable_spam_message {
            let current = self.message_spam_cooldown.load(Ordering::Relaxed);
            if current > 0 {
                self.message_spam_cooldown
                    .store(current - 1, Ordering::Relaxed);
            } else {
                let min_d = args.spam_message_delay_min.min(args.spam_message_delay_max);
                let max_d = args.spam_message_delay_max.max(args.spam_message_delay_min);
                let next_cooldown = {
                    let mut rng = rand::rng();
                    rng.random_range(min_d..=max_d)
                };
                self.message_spam_cooldown
                    .store(next_cooldown, Ordering::Relaxed);

                let count = self.message_count.fetch_add(1, Ordering::Relaxed);
                let base = args
                    .spam_message
                    .as_deref()
                    .unwrap_or("Please do not spam!");
                let message = format!("{base} #{count}");

                let mut payload = Vec::new();
                let text = SText::new(message, self.username.clone());
                if PacketWrite::write(&text, &mut payload).is_ok() {
                    let _ = self.send_game_packet(9, &payload, true).await;
                }
            }
        }
    }

    fn tick_physics(&self, args: &Args) {
        let mut x = self.current_x.load();
        let mut y = self.current_y.load();
        let mut z = self.current_z.load();
        let ground_y = self.ground_y.load();

        let mut vx = self.velocity_x.load();
        let mut vy = self.velocity_y.load();
        let mut vz = self.velocity_z.load();

        if args.enable_movement {
            let mut rng = rand::rng();
            if self.is_walking.load(Ordering::Relaxed) {
                let ticks = self.move_ticks.load(Ordering::Relaxed);
                if ticks > 0 {
                    self.move_ticks.store(ticks - 1, Ordering::Relaxed);
                    vx = self.move_dir_x.load() * 0.2;
                    vz = self.move_dir_z.load() * 0.2;

                    if args.enable_rotation {
                        let target_yaw = ((-vx).atan2(vz) as f32).to_degrees();
                        self.last_sent_yaw.store(target_yaw);
                    }

                    if args.enable_jumping && self.on_ground.load(Ordering::Relaxed) {
                        let j_cd = self.jump_cooldown.load(Ordering::Relaxed);
                        if j_cd > 0 {
                            self.jump_cooldown.store(j_cd - 1, Ordering::Relaxed);
                        } else if rng.random_bool(0.08) {
                            vy = 0.42;
                            self.on_ground.store(false, Ordering::Relaxed);
                            self.jump_cooldown
                                .store(rng.random_range(10..25), Ordering::Relaxed);
                        }
                    }
                } else {
                    self.is_walking.store(false, Ordering::Relaxed);
                    self.move_cooldown
                        .store(rng.random_range(20..80), Ordering::Relaxed);
                }
            } else {
                let cd = self.move_cooldown.load(Ordering::Relaxed);
                if cd > 0 {
                    self.move_cooldown.store(cd - 1, Ordering::Relaxed);
                    vx *= 0.5;
                    vz *= 0.5;
                } else {
                    self.is_walking.store(true, Ordering::Relaxed);
                    self.move_ticks
                        .store(rng.random_range(30..100), Ordering::Relaxed);
                    let angle: f64 = rng.random_range(0.0..std::f64::consts::TAU);
                    self.move_dir_x.store(angle.cos());
                    self.move_dir_z.store(angle.sin());
                }
            }
        }

        if !self.on_ground.load(Ordering::Relaxed) {
            vy = (vy - 0.08) * 0.98;
        }

        x += vx;
        y += vy;
        z += vz;

        if y <= ground_y {
            y = ground_y;
            vy = 0.0;
            self.on_ground.store(true, Ordering::Relaxed);
        }

        vx *= 0.91;
        vz *= 0.91;

        self.current_x.store(x);
        self.current_y.store(y);
        self.current_z.store(z);
        self.velocity_x.store(vx);
        self.velocity_y.store(vy);
        self.velocity_z.store(vz);
    }

    async fn send_auth_input(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let x = self.current_x.load();
        let y = self.current_y.load();
        let z = self.current_z.load();

        let last_x = self.last_sent_x.load();
        let last_y = self.last_sent_y.load();
        let last_z = self.last_sent_z.load();

        let delta_x = (x - last_x) as f32;
        let delta_y = (y - last_y) as f32;
        let delta_z = (z - last_z) as f32;

        self.last_sent_x.store(x);
        self.last_sent_y.store(y);
        self.last_sent_z.store(z);

        let pitch = self.last_sent_pitch.load();
        let yaw = self.last_sent_yaw.load();
        let tick = self.tick_counter.load(Ordering::Relaxed);

        // Bedrock reports position at eye height (+1.62)
        let eye_y = (y + 1.62) as f32;

        let mut payload = Vec::new();
        // Pitch, Yaw
        payload.extend_from_slice(&pitch.to_le_bytes());
        payload.extend_from_slice(&yaw.to_le_bytes());
        // Position (Vector3<f32>)
        payload.extend_from_slice(&(x as f32).to_le_bytes());
        payload.extend_from_slice(&eye_y.to_le_bytes());
        payload.extend_from_slice(&(z as f32).to_le_bytes());
        // move_vec (Vector2<f32>)
        payload.extend_from_slice(&0.0f32.to_le_bytes());
        payload.extend_from_slice(&0.0f32.to_le_bytes());
        // head_yaw
        payload.extend_from_slice(&yaw.to_le_bytes());
        // input_data flags count: 0
        VarUInt(0).encode(&mut payload)?;
        // input_mode: 0, play_mode: 0, interaction_model: -1
        VarUInt(0).encode(&mut payload)?;
        VarUInt(0).encode(&mut payload)?;
        VarInt(-1).encode(&mut payload)?;
        // interact_pitch, interact_yaw
        payload.extend_from_slice(&0.0f32.to_le_bytes());
        payload.extend_from_slice(&0.0f32.to_le_bytes());
        // tick
        VarULong(tick).encode(&mut payload)?;
        // delta (Vector3<f32>)
        payload.extend_from_slice(&delta_x.to_le_bytes());
        payload.extend_from_slice(&delta_y.to_le_bytes());
        payload.extend_from_slice(&delta_z.to_le_bytes());
        // 5 optional action flags (all None / false)
        payload.extend(std::iter::repeat_n(0u8, 5));
        // Trailing analog_move (2 floats), camera_orientation (3 floats), raw_move (2 floats)
        for _ in 0..7 {
            payload.extend_from_slice(&0.0f32.to_le_bytes());
        }

        self.send_game_packet(144, &payload, false).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_protocol::bedrock::server::player_auth_input::SPlayerAuthInput;
    use pumpkin_protocol::serial::PacketRead;

    #[test]
    fn test_remove_component_two_candidates() {
        let sdp = "v=0\r\na=candidate:1 1 udp 2130706431 127.0.0.1 5000 typ host\r\na=candidate:2 2 udp 2130706430 127.0.0.1 5001 typ host\r\na=end-of-candidates\r\n";
        let filtered = remove_component_two_candidates(sdp);
        assert!(filtered.contains("a=candidate:1 1 udp 2130706431"));
        assert!(!filtered.contains("a=candidate:2 2 udp"));
        assert!(filtered.contains("a=end-of-candidates"));
    }

    #[test]
    fn test_fragment_buffer_single_chunk() {
        let mut buffer = FragmentBuffer::default();
        let payload = b"complete packet";
        let result = buffer.push(0, payload);
        assert_eq!(result, Some(payload.to_vec()));
    }

    #[test]
    fn test_fragment_buffer_multi_chunk() {
        let mut buffer = FragmentBuffer::default();
        // 3 fragments: remaining counts are 2, 1, 0
        assert_eq!(buffer.push(2, b"part1"), None);
        assert_eq!(buffer.push(1, b"part2"), None);
        assert_eq!(buffer.push(0, b"part3"), Some(b"part1part2part3".to_vec()));
    }

    #[test]
    fn test_fragment_buffer_out_of_order_resets() {
        let mut buffer = FragmentBuffer::default();
        assert_eq!(buffer.push(2, b"part1"), None);
        // Expecting 1, but getting 0 or 2
        assert_eq!(buffer.push(0, b"part3"), None);
        assert_eq!(buffer.next_remaining, None);
    }

    #[test]
    fn test_auth_input_encoding_and_decoding() {
        let mut payload = Vec::new();
        let pitch = 15.0f32;
        let yaw = 90.0f32;
        let x = 100.5f32;
        let eye_y = 65.62f32;
        let z = -200.25f32;
        let tick = 42u64;
        let delta_x = 0.1f32;
        let delta_y = -0.05f32;
        let delta_z = 0.2f32;

        payload.extend_from_slice(&pitch.to_le_bytes());
        payload.extend_from_slice(&yaw.to_le_bytes());
        payload.extend_from_slice(&x.to_le_bytes());
        payload.extend_from_slice(&eye_y.to_le_bytes());
        payload.extend_from_slice(&z.to_le_bytes());
        payload.extend_from_slice(&0.0f32.to_le_bytes()); // move_vec.x
        payload.extend_from_slice(&0.0f32.to_le_bytes()); // move_vec.y
        payload.extend_from_slice(&yaw.to_le_bytes()); // head_yaw
        VarUInt(0).encode(&mut payload).unwrap();
        VarUInt(0).encode(&mut payload).unwrap();
        VarUInt(0).encode(&mut payload).unwrap();
        VarInt(-1).encode(&mut payload).unwrap();
        payload.extend_from_slice(&0.0f32.to_le_bytes());
        payload.extend_from_slice(&0.0f32.to_le_bytes());
        VarULong(tick).encode(&mut payload).unwrap();
        payload.extend_from_slice(&delta_x.to_le_bytes());
        payload.extend_from_slice(&delta_y.to_le_bytes());
        payload.extend_from_slice(&delta_z.to_le_bytes());
        payload.extend(std::iter::repeat_n(0u8, 5));
        for _ in 0..7 {
            payload.extend_from_slice(&0.0f32.to_le_bytes());
        }

        let mut reader = payload.as_slice();
        let packet = SPlayerAuthInput::read(&mut reader).expect("Decode SPlayerAuthInput");
        assert_eq!(packet.pitch, pitch);
        assert_eq!(packet.yaw, yaw);
        assert_eq!(packet.position.x, x);
        assert_eq!(packet.position.y, eye_y);
        assert_eq!(packet.position.z, z);
        assert_eq!(packet.tick.0, tick);
        assert_eq!(packet.delta.x, delta_x);
        assert_eq!(packet.delta.y, delta_y);
        assert_eq!(packet.delta.z, delta_z);
    }

    #[tokio::test]
    async fn test_bedrock_batch_compression_cycle() {
        let mut encoder = BedrockBatchEncoder::new();
        encoder.set_compression((128, 6));

        let packet_id = 9u16; // SText
        let sample_text = b"Benchmark Test Chat Message from Bedrock Bot";
        let mut encoded = Vec::new();

        encoder
            .write_game_packet(
                packet_id,
                SubClient::Main,
                SubClient::Main,
                sample_text,
                &mut encoded,
            )
            .unwrap();

        assert_eq!(encoded[0], BEDROCK_GAME_PACKET);

        let mut decoder = BedrockBatchDecoder::new();
        decoder.set_compression(128);

        let decompressed = decoder.get_packet_payload(encoded).await.unwrap();
        let mut cursor = Cursor::new(decompressed);
        let raw = decoder.get_game_packet(&mut cursor).unwrap();

        assert_eq!(raw.id, packet_id as i32);
        assert_eq!(raw.payload.as_ref(), sample_text);
    }
}

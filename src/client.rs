use crossbeam::atomic::AtomicCell;
use pumpkin_protocol::bytebuf::ReadingError;
use pumpkin_protocol::bytebuf::packet::Packet;
use pumpkin_protocol::client::config::{CConfigDisconnect, CFinishConfig};
use pumpkin_protocol::client::login::{
    CEncryptionRequest, CLoginDisconnect, CLoginSuccess, CSetCompression,
};
use pumpkin_protocol::client::play::{CKeepAlive, CPlayDisconnect, CPlayerPosition};
use pumpkin_protocol::codec::var_int::VarInt;
use pumpkin_protocol::server::config::{SAcknowledgeFinishConfig, SKnownPacks};
use pumpkin_protocol::server::handshake::SHandShake;
use pumpkin_protocol::server::login::{SLoginAcknowledged, SLoginStart};
use pumpkin_protocol::server::play::{SConfirmTeleport, SKeepAlive};
use pumpkin_protocol::{CURRENT_MC_PROTOCOL, RawPacket, ServerPacket};
use pumpkin_protocol::{
    ClientPacket, CompressionLevel, CompressionThreshold, ConnectionState,
    packet_decoder::PacketDecoder, packet_encoder::PacketEncoder,
};
use std::collections::VecDeque;
use std::sync::{Arc, atomic::AtomicBool};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::{
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::Mutex,
};
use uuid::Uuid;

/// Everything which makes a Connection with our Server is a `Client`.
pub struct Client {
    /// The current connection state of the client (e.g., Handshaking, Status, Play).
    pub connection_state: AtomicCell<ConnectionState>,
    /// Indicates if the client connection is closed.
    pub closed: AtomicBool,
    /// The packet encoder for outgoing packets.
    pub enc: Arc<Mutex<PacketEncoder>>,
    /// The packet decoder for incoming packets.
    pub dec: Arc<Mutex<PacketDecoder>>,
    pub connection_reader: Mutex<OwnedReadHalf>,
    pub connection_writer: Mutex<OwnedWriteHalf>,
    pub client_packets_queue: Arc<Mutex<VecDeque<RawPacket>>>,
    // Its read-only and not needs AtomicBool
    realistic: bool
}

impl Client {
    pub fn new(stream: TcpStream, realistic: bool) -> Self {
        let (connection_reader, connection_writer) = stream.into_split();
        Self {
            connection_state: AtomicCell::new(ConnectionState::HandShake),
            enc: Arc::new(Mutex::new(PacketEncoder::default())),
            dec: Arc::new(Mutex::new(PacketDecoder::default())),
            closed: AtomicBool::new(false),
            connection_reader: Mutex::new(connection_reader),
            connection_writer: Mutex::new(connection_writer),
            client_packets_queue: Arc::new(Mutex::new(VecDeque::new())),
            realistic,
        }
    }

    /// Enables or disables packet compression for the connection.
    ///
    /// This function takes an optional `CompressionInfo` struct as input. If the `CompressionInfo` is provided,
    /// packet compression is enabled with the specified threshold. Otherwise, compression is disabled.
    ///
    /// # Arguments
    ///
    /// * `compression`: An optional `CompressionInfo` struct containing the compression threshold and compression level.
    pub async fn set_compression(
        &self,
        compression: Option<(CompressionThreshold, CompressionLevel)>,
    ) {
        self.dec.lock().await.set_compression(compression.is_some());
        self.enc
            .lock()
            .await
            .set_compression(compression.map(|s| (s.0, s.1)))
            .unwrap_or_else(|_| log::warn!("invalid compression level"));
    }

    pub async fn process_packets(&self) {
        let mut packet_queue = self.client_packets_queue.lock().await;
        while let Some(mut packet) = packet_queue.pop_front() {
            if self.closed.load(std::sync::atomic::Ordering::Relaxed) {
                log::debug!("Canceling client packet processing (pre)");
                return;
            }
            if let Err(error) = self.handle_packet(&mut packet).await {
                log::error!(
                    "Failed to read incoming packet with id {}: {}",
                    i32::from(packet.id),
                    error
                );
                self.close().await;
            };
        }
    }
    pub async fn add_packet(&self, packet: RawPacket) {
        let mut client_packets_queue = self.client_packets_queue.lock().await;
        client_packets_queue.push_back(packet);
    }

    pub async fn poll(&self) -> bool {
        loop {
            if self.closed.load(std::sync::atomic::Ordering::Relaxed) {
                // If we manually close (like a kick) we dont want to keep reading bytes
                return false;
            }

            let mut dec = self.dec.lock().await;

            match dec.decode() {
                Ok(Some(packet)) => {
                    self.add_packet(packet).await;
                    return true;
                }
                Ok(None) => (), //log::debug!("Waiting for more data to complete packet..."),
                Err(err) => {
                    log::warn!("Failed to decode packet for: {}", err.to_string());
                    self.close().await;
                    return false; // return to avoid reserving additional bytes
                }
            }

            dec.reserve(4096);
            let mut buf = dec.take_capacity();

            let bytes_read = self.connection_reader.lock().await.read_buf(&mut buf).await;
            match bytes_read {
                Ok(cnt) => {
                    //log::debug!("Read {} bytes", cnt);
                    if cnt == 0 {
                        self.close().await;
                        return false;
                    }
                }
                Err(error) => {
                    log::error!("Error while reading incoming packet {}", error);
                    self.close().await;
                    return false;
                }
            };

            // This should always be an O(1) unsplit because we reserved space earlier and
            // the call to `read_buf` shouldn't have grown the allocation.
            dec.queue_bytes(buf);
        }
    }

    /// Sends a clientbound packet to the connected client.
    ///
    /// # Arguments
    ///
    /// * `packet`: A reference to a packet object implementing the `ClientPacket` trait.
    pub async fn send_packet<P: ClientPacket>(&self, packet: &P) {
        //log::debug!("Sending packet with id {} to {}", P::PACKET_ID, self.id);
        // assert!(!self.closed);
        if self.closed.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }

        let mut enc = self.enc.lock().await;
        if let Err(_error) = enc.append_packet(packet) {
            return;
        }

        let mut writer = self.connection_writer.lock().await;
        if let Err(error) = writer.write_all(&enc.take()).await {
            log::debug!("Unable to write to connection: {}", error.to_string());
        }
    }

    pub async fn join_server(&self, ip: String, port: u16, name: String) {
        self.send_packet(&SHandShake {
            protocol_version: VarInt(CURRENT_MC_PROTOCOL.get() as i32),
            server_address: ip,
            server_port: port,
            next_state: pumpkin_protocol::ConnectionState::Login,
        })
        .await;
        self.connection_state.store(ConnectionState::Login);
        self.send_packet(&SLoginStart {
            name,
            uuid: Uuid::new_v4(),
        })
        .await;
    }

    pub async fn handle_packet(&self, packet: &mut RawPacket) -> Result<(), ReadingError> {
        match self.connection_state.load() {
            ConnectionState::HandShake => unreachable!(),
            ConnectionState::Status => todo!(),
            ConnectionState::Login => self.handle_login_packet(packet).await?,
            ConnectionState::Transfer => log::debug!("Got packet in transfer state"),
            ConnectionState::Config => self.handle_config_packet(packet).await?,
            ConnectionState::Play => self.handle_play_packet(packet).await?,
        };
        Ok(())
    }

    async fn handle_login_packet(&self, packet: &mut RawPacket) -> Result<(), ReadingError> {
        let bytebuf = &mut packet.bytebuf;
        match packet.id.0 {
            CEncryptionRequest::PACKET_ID => {
                log::debug!("Got Encryption Request")
            }
            CSetCompression::PACKET_ID => {
                log::trace!("Set Compression");
                let packet = CSetCompression::read(bytebuf)?;
                self.set_compression(Some((
                    CompressionThreshold(packet.threshold.0 as u32),
                    CompressionLevel(6),
                )))
                .await
            }
            CLoginDisconnect::PACKET_ID => {
                log::error!("Kicking in Login State with reason:\n{}", String::from_utf8_lossy(packet.bytebuf.iter().as_slice()));
                self.close().await;
            }
            CLoginSuccess::PACKET_ID => {
                log::trace!("Login -> Config");
                self.send_packet(&SLoginAcknowledged).await;
                self.connection_state.store(ConnectionState::Config);
                log::trace!("Sending Known packs");
                self.send_packet(&SKnownPacks {
                    known_pack_count: VarInt(0),
                })
                .await;
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_config_packet(&self, packet: &mut RawPacket) -> Result<(), ReadingError> {
        match packet.id.0 {
            CConfigDisconnect::PACKET_ID => {
                log::error!("Kicking in Config State");
                self.close().await;
            }
            CFinishConfig::PACKET_ID => {
                log::trace!("Config -> Play");
                self.send_packet(&SAcknowledgeFinishConfig).await;
                self.connection_state.store(ConnectionState::Play);
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_play_packet(&self, packet: &mut RawPacket) -> Result<(), ReadingError> {
        let bytebuf = &mut packet.bytebuf;
        match packet.id.0 {
            CKeepAlive::PACKET_ID => {
                let packet = CKeepAlive::read(bytebuf)?;
                self.send_packet(&SKeepAlive {
                    keep_alive_id: packet.keep_alive_id,
                })
                .await;
            }
            CPlayerPosition::PACKET_ID => {
                let packet = CPlayerPosition::read(bytebuf)?;
                self.send_packet(&SConfirmTeleport {
                    teleport_id: packet.teleport_id,
                })
                .await;
            }
            CPlayDisconnect::PACKET_ID => {
                log::error!("Kicking in Play State");
                self.close().await;
            }
            _ => {}
        }
        Ok(())
    }

    pub async fn close(&self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

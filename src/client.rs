use crossbeam::atomic::AtomicCell;
use pumpkin_data::packet::CURRENT_MC_PROTOCOL;
use pumpkin_protocol::codec::var_int::VarInt;
use pumpkin_protocol::java::client::config::{CConfigDisconnect, CFinishConfig};
use pumpkin_protocol::java::client::login::{
    CEncryptionRequest, CLoginDisconnect, CLoginSuccess, CSetCompression,
};
use pumpkin_protocol::java::client::play::{CKeepAlive, CPlayDisconnect, CPlayerPosition};
use pumpkin_protocol::java::packet_decoder::TCPNetworkDecoder;
use pumpkin_protocol::java::packet_encoder::TCPNetworkEncoder;
use pumpkin_protocol::java::server::config::{SAcknowledgeFinishConfig, SKnownPacks};
use pumpkin_protocol::java::server::handshake::SHandShake;
use pumpkin_protocol::java::server::login::{SLoginAcknowledged, SLoginStart};
use pumpkin_protocol::java::server::play::{SChatMessage, SConfirmTeleport, SKeepAlive};
use pumpkin_protocol::packet::Packet;
use pumpkin_protocol::ser::NetworkWriteExt;
use pumpkin_protocol::ser::{ReadingError, WritingError};
use pumpkin_protocol::{
    ClientPacket, CompressionLevel, CompressionThreshold, ConnectionState, PacketDecodeError,
    RawPacket, ServerPacket,
};
use std::io::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{BufReader, BufWriter};
use tokio::sync::Notify;
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
    pub network_writer: Arc<Mutex<TCPNetworkEncoder<BufWriter<OwnedWriteHalf>>>>,
    /// The packet decoder for incoming packets.
    pub network_reader: Arc<Mutex<TCPNetworkDecoder<BufReader<OwnedReadHalf>>>>,
    close_interrupt: Arc<Notify>,

    message_spam_cooldown: AtomicU32,
}

impl Client {
    pub fn new(stream: TcpStream) -> Self {
        let (connection_reader, connection_writer) = stream.into_split();
        Self {
            connection_state: AtomicCell::new(ConnectionState::HandShake),
            network_writer: Arc::new(Mutex::new(TCPNetworkEncoder::new(BufWriter::new(
                connection_writer,
            )))),
            network_reader: Arc::new(Mutex::new(TCPNetworkDecoder::new(BufReader::new(
                connection_reader,
            )))),
            closed: AtomicBool::new(false),
            close_interrupt: Arc::new(Notify::new()),
            message_spam_cooldown: AtomicU32::new(1),
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
        if let Some(compression) = compression {
            self.network_reader
                .lock()
                .await
                .set_compression(compression.0);
            self.network_writer
                .lock()
                .await
                .set_compression(compression);
        }
    }

    pub async fn await_close_interrupt(&self) {
        self.close_interrupt.notified().await;
    }

    pub async fn get_packet(&self) -> Option<RawPacket> {
        let mut network_reader = self.network_reader.lock().await;
        tokio::select! {
            () = self.await_close_interrupt() => {
                log::debug!("Canceling player packet processing");
                None
            },
            packet_result = network_reader.get_raw_packet() => {
                match packet_result {
                    Ok(packet) => Some(packet),
                    Err(err) => {
                        if !matches!(err, PacketDecodeError::ConnectionClosed) {
                            log::warn!("Failed to decode packet from client: {err}");
                            self.close().await;
                        }
                        None
                    }
                }
            }
        }
    }

    pub async fn process_packets(self: &Arc<Self>) -> bool {
        let packet = self.get_packet().await;
        let Some(mut packet) = packet else {
            return false;
        };

        if let Err(error) = self.handle_packet(&mut packet).await {
            log::error!(
                "Failed to read incoming packet with id {}: {}",
                packet.id,
                error
            );
            self.close().await;
        }
        true
    }

    // TODO: make this less ugly ig
    pub async fn tick(&self, spam_message: Arc<Option<String>>, spam_message_delay: u32) {
        if self.connection_state.load() != ConnectionState::Play {
            return;
        }
        if let Some(spam_message) = spam_message.as_ref()
            && self
                .message_spam_cooldown
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed)
                == 0
        {
            self.message_spam_cooldown
                .store(spam_message_delay, std::sync::atomic::Ordering::Relaxed);
            self.send_message(spam_message.clone()).await
        }
    }

    pub async fn send_message(&self, message: String) {
        let start = SystemTime::now();
        let since_the_epoch = start
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards");
        self.send_packet(&SChatMessage {
            message,
            timestamp: since_the_epoch.as_millis() as i64,
            salt: rand::random(),
            signature: None,
            message_count: VarInt(1),
            acknowledged: vec![0; 20].into_boxed_slice(),
            checksum: 0,
        })
        .await;
    }

    pub fn write_packet<P: ClientPacket>(
        packet: &P,
        write: impl Write,
    ) -> Result<(), WritingError> {
        let mut write = write;
        write.write_var_int(&VarInt(P::PACKET_ID))?;
        packet.write_packet_data(write)
    }

    /// Sends a clientbound packet to the connected client.
    ///
    /// # Arguments
    ///
    /// * `packet`: A reference to a packet object implementing the `ClientPacket` trait.
    pub async fn send_packet<P: ClientPacket>(&self, packet: &P) {
        let mut packet_buf = Vec::new();
        let writer = &mut packet_buf;
        Self::write_packet(packet, writer).unwrap();
        if let Err(err) = self
            .network_writer
            .lock()
            .await
            .write_packet(packet_buf.into())
            .await
        {
            // It is expected that the packet will fail if we are closed
            if !self.closed.load(Ordering::Relaxed) {
                log::warn!("Failed to send packet to client: {err}");
                // We now need to close the connection to the client since the stream is in an
                // unknown state
                self.close().await;
            }
        }
    }

    pub async fn join_server(&self, address: SocketAddr, name: String) {
        dbg!(address.ip().to_string());
        self.send_packet(&SHandShake {
            protocol_version: VarInt(CURRENT_MC_PROTOCOL as i32),
            server_address: address.ip().to_string(),
            server_port: address.port(),
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
        let bytebuf = &packet.payload[..];
        match packet.id {
            CEncryptionRequest::PACKET_ID => {
                log::debug!("Got Encryption Request")
            }
            CSetCompression::PACKET_ID => {
                log::trace!("Set Compression");
                let packet = CSetCompression::read(bytebuf)?;
                self.set_compression(Some((packet.threshold.0 as usize, 6)))
                    .await
            }
            CLoginDisconnect::PACKET_ID => {
                log::error!("Kicking in Login State");
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
        match packet.id {
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
        let bytebuf = &packet.payload[..];
        match packet.id {
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
        self.close_interrupt.notify_waiters();
        self.closed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

use crossbeam::atomic::AtomicCell;
use pumpkin_data::packet::CURRENT_MC_PROTOCOL;
use pumpkin_protocol::codec::var_int::VarInt;
use pumpkin_protocol::java::client::config::{CConfigDisconnect, CFinishConfig};
use pumpkin_protocol::java::client::login::{
    CEncryptionRequest, CLoginDisconnect, CLoginSuccess, CSetCompression,
};
use pumpkin_protocol::java::client::play::{CKeepAlive, CLogin, CPlayDisconnect, CPlayerPosition};
use pumpkin_protocol::java::packet_decoder::TCPNetworkDecoder;
use pumpkin_protocol::java::packet_encoder::TCPNetworkEncoder;
use pumpkin_protocol::java::server::config::{SAcknowledgeFinishConfig, SKnownPacks};
use pumpkin_protocol::java::server::handshake::SHandShake;
use pumpkin_protocol::java::server::login::{SLoginAcknowledged, SLoginStart};
use pumpkin_protocol::java::server::play::{
    SChatMessage, SConfirmTeleport, SKeepAlive, SPlayerLoaded, SPlayerPosition, SPlayerRotation,
    SSwingArm,
};
use pumpkin_protocol::packet::MultiVersionJavaPacket;
use pumpkin_protocol::ser::NetworkWriteExt;
use pumpkin_protocol::ser::{ReadingError, WritingError};
use pumpkin_protocol::{
    ClientPacket, CompressionLevel, CompressionThreshold, ConnectionState, PacketDecodeError,
    RawPacket, ServerPacket,
};
use pumpkin_util::math::vector3::Vector3;
use pumpkin_util::version::MinecraftVersion;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
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

use crate::Args;

/// Everything which makes a Connection with our Server is a `Client`.
#[expect(dead_code)]
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

    entity_id: AtomicI32,

    message_spam_cooldown: AtomicU32,
    message_count: AtomicU32,
    is_loaded: AtomicBool,
    swing_cooldown: AtomicU32,
    // Position
    current_x: AtomicCell<f64>,
    current_y: AtomicCell<f64>,
    current_z: AtomicCell<f64>,

    velocity_x: AtomicCell<f64>,
    velocity_y: AtomicCell<f64>,
    velocity_z: AtomicCell<f64>,

    start_x: AtomicCell<f64>,
    start_z: AtomicCell<f64>,
    target_x: AtomicCell<f64>,
    target_z: AtomicCell<f64>,

    move_progress: AtomicCell<f32>,
    move_cooldown: AtomicU32,

    // Rotation
    current_yaw: AtomicCell<f32>,
    current_pitch: AtomicCell<f32>,
    // Starting point of the movement
    start_yaw: AtomicCell<f32>,
    start_pitch: AtomicCell<f32>,
    // Goal point
    target_yaw: AtomicCell<f32>,
    target_pitch: AtomicCell<f32>,
    // Progress: 0.0 (start) to 1.0 (end)
    rotation_progress: AtomicCell<f32>,
    rotation_cooldown: AtomicU32,
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
            entity_id: AtomicI32::new(0),
            velocity_x: AtomicCell::new(0.0),
            velocity_y: AtomicCell::new(0.0),
            velocity_z: AtomicCell::new(0.0),

            closed: AtomicBool::new(false),
            swing_cooldown: AtomicU32::new(0),
            close_interrupt: Arc::new(Notify::new()),
            message_spam_cooldown: AtomicU32::new(1),
            message_count: AtomicU32::new(0),
            is_loaded: AtomicBool::new(false),
            rotation_cooldown: AtomicU32::new(0),
            rotation_progress: AtomicCell::new(1.0),
            current_yaw: AtomicCell::new(0.0),
            current_pitch: AtomicCell::new(0.0),
            start_yaw: AtomicCell::new(0.0),
            start_pitch: AtomicCell::new(0.0),
            target_yaw: AtomicCell::new(0.0),
            target_pitch: AtomicCell::new(0.0),

            current_x: AtomicCell::new(0.0),
            current_y: AtomicCell::new(0.0),
            current_z: AtomicCell::new(0.0),
            start_x: AtomicCell::new(0.0),
            start_z: AtomicCell::new(0.0),
            target_x: AtomicCell::new(0.0),
            target_z: AtomicCell::new(0.0),
            move_progress: AtomicCell::new(1.0),
            move_cooldown: AtomicU32::new(0),
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
                            log::warn!("Failed to decode packet from server: {err}");
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
    pub async fn tick(&self, args: &Args) {
        if self.connection_state.load() != ConnectionState::Play {
            return;
        }
        if !self.is_loaded.load(Ordering::Relaxed) {
            return;
        }

        if let Some(spam_message) = &args.spam_message {
            let result = self.message_spam_cooldown.fetch_update(
                Ordering::SeqCst,
                Ordering::Relaxed,
                |curr| {
                    if curr == 0 {
                        let delay = rand::random_range(
                            args.spam_message_delay_min..args.spam_message_delay_max,
                        );
                        Some(delay)
                    } else {
                        Some(curr - 1)
                    }
                },
            );
            if let Ok(0) = result {
                self.send_message(spam_message.clone()).await;
            }
        }

        if args.enable_rotation {
            self.tick_rotation().await;
        }
        if args.enable_swing {
            self.tick_swing().await;
        }
        //self.tick_movement().await
    }

    async fn tick_rotation(&self) {
        let progress = self.rotation_progress.load();
        let cooldown = self.rotation_cooldown.load(Ordering::Relaxed);

        if progress >= 1.0 {
            // We have finished the movement. Wait for the cooldown.
            if cooldown == 0 {
                // Pick a new target and reset
                let current_y = self.current_yaw.load();
                let current_p = self.current_pitch.load();

                self.start_yaw.store(current_y);
                self.start_pitch.store(current_p);
                self.target_yaw.store(rand::random_range(-180.0..180.0));
                self.target_pitch.store(rand::random_range(-90.0..90.0));

                self.rotation_progress.store(0.0);
                // Wait for 2-5 seconds (40-100 ticks) before moving again
                self.rotation_cooldown
                    .store(rand::random_range(40..100), Ordering::Relaxed);
            } else {
                self.rotation_cooldown.fetch_sub(1, Ordering::Relaxed);
            }
        } else {
            // Increment progress (e.g., 0.02 means it takes 50 ticks/2.5s to complete turn)
            let new_progress = (progress + 0.02).min(1.0);
            self.rotation_progress.store(new_progress);

            // Calculate the "Smooth" T value using the S-Curve formula
            // t = 3p^2 - 2p^3
            let t = 3.0 * new_progress.powi(2) - 2.0 * new_progress.powi(3);

            // Interpolate: start + (target - start) * t
            let start_y = self.start_yaw.load();
            let target_y = self.target_yaw.load();
            let interpolated_yaw = start_y + (target_y - start_y) * t;

            let start_p = self.start_pitch.load();
            let target_p = self.target_pitch.load();
            let interpolated_pitch = start_p + (target_p - start_p) * t;

            self.current_yaw.store(interpolated_yaw);
            self.current_pitch.store(interpolated_pitch);

            // Send rotation packet
            self.send_packet(&SPlayerRotation {
                yaw: interpolated_yaw,
                pitch: interpolated_pitch,
                ground: true,
            })
            .await;
        }
    }

    #[expect(dead_code)]
    async fn tick_movement(&self) {
        let progress = self.move_progress.load();
        let cooldown = self.move_cooldown.load(Ordering::Relaxed);

        if progress >= 1.0 {
            if cooldown == 0 {
                // Start a new walk
                let cur_x = self.current_x.load();
                let cur_z = self.current_z.load();

                self.start_x.store(cur_x);
                self.start_z.store(cur_z);

                // Small random walk (0.5 to 1.5 blocks)
                let offset_x = rand::random_range(-1.5..1.5);
                let offset_z = rand::random_range(-1.5..1.5);

                self.target_x.store(cur_x + offset_x);
                self.target_z.store(cur_z + offset_z);

                self.move_progress.store(0.0);
                // Wait 2-5 seconds between moves
                self.move_cooldown
                    .store(rand::random_range(40..100), Ordering::Relaxed);
            } else {
                self.move_cooldown.fetch_sub(1, Ordering::Relaxed);
            }
        } else {
            // Increase this to 0.05 for a normal human walking pace (20 ticks to finish)
            let new_progress = (progress + 0.05).min(1.0);
            self.move_progress.store(new_progress);

            // Cubic easing for a natural start/stop
            let t = 3.0 * new_progress.powi(2) - 2.0 * new_progress.powi(3);

            let start_x = self.start_x.load();
            let target_x = self.target_x.load();
            let start_z = self.start_z.load();
            let target_z = self.target_z.load();

            let interp_x = start_x + (target_x - start_x) * t as f64;
            let interp_z = start_z + (target_z - start_z) * t as f64;

            self.current_x.store(interp_x);
            self.current_z.store(interp_z);

            self.send_packet(&SPlayerPosition {
                position: Vector3::new(interp_x, self.current_y.load(), interp_z),
                collision: 1, // on_ground
            })
            .await;
        }
    }

    async fn tick_swing(&self) {
        let cooldown = self.swing_cooldown.load(Ordering::Relaxed);

        if cooldown == 0 {
            if rand::random_bool(0.01) {
                self.send_packet(&SSwingArm { hand: VarInt(0) }).await;

                self.swing_cooldown
                    .store(rand::random_range(20..40), Ordering::Relaxed);
            }
        } else {
            self.swing_cooldown.fetch_sub(1, Ordering::Relaxed);
        }
    }

    pub async fn send_message(&self, message: String) {
        let count = self.message_count.fetch_add(1, Ordering::SeqCst);
        let start = SystemTime::now();
        let since_the_epoch = start
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards");
        self.send_packet(&SChatMessage {
            message,
            timestamp: since_the_epoch.as_millis() as i64,
            salt: rand::random(),
            signature: None,
            message_count: VarInt(count as i32),
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
        write.write_var_int(&VarInt(P::PACKET_ID.latest_id))?;
        packet.write_packet_data(write, &MinecraftVersion::V_1_21_11)
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
            id if id == CEncryptionRequest::PACKET_ID => {
                todo!("Encryption is currently not implemented, please disable it at server level")
            }
            id if id == CSetCompression::PACKET_ID => {
                log::trace!("Set Compression");
                let packet = CSetCompression::read(bytebuf)?;
                self.set_compression(Some((packet.threshold.0 as usize, 6)))
                    .await
            }
            id if id == CLoginDisconnect::PACKET_ID => {
                log::error!("Kicking in Login State");
                self.close().await;
            }
            id if id == CLoginSuccess::PACKET_ID => {
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
            id if id == CConfigDisconnect::PACKET_ID => {
                log::error!("Kicking in Config State");
                self.close().await;
            }
            id if id == CFinishConfig::PACKET_ID => {
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
            id if id == CKeepAlive::PACKET_ID => {
                let packet = CKeepAlive::read(bytebuf)?;
                self.send_packet(&SKeepAlive {
                    keep_alive_id: packet.keep_alive_id,
                })
                .await;
            }
            // TODO
            // id if id == CEntityVelocity::PACKET_ID => {
            // let packet = CEntityVelocity::read(bytebuf)?;

            //     if packet.entity_id.0 == self.entity_id.load(Ordering::Relaxed) as i32 {
            //         self.velocity_x.store(packet.velocity.0.x as f64 / 8000.0);
            //         self.velocity_y.store(packet.velocity.0.y as f64 / 8000.0);
            //         self.velocity_z.store(packet.velocity.0.z as f64 / 8000.0);
            //     }
            // }
            id if id == CPlayerPosition::PACKET_ID => {
                let packet = CPlayerPosition::read(bytebuf)?;
                self.current_yaw.store(packet.yaw);
                self.current_pitch.store(packet.pitch);

                let x = packet.position.x;
                let y = packet.position.y;
                let z = packet.position.z;

                self.current_x.store(x);
                self.current_y.store(y);
                self.current_z.store(z);

                self.start_x.store(x);
                self.start_z.store(z);
                self.target_x.store(x);
                self.target_z.store(z);

                self.move_progress.store(1.0);
                self.move_cooldown
                    .store(rand::random_range(40..100), Ordering::Relaxed);
                self.send_packet(&SConfirmTeleport {
                    teleport_id: packet.teleport_id,
                })
                .await;
                self.is_loaded.store(true, Ordering::Relaxed);
            }
            id if id == CLogin::PACKET_ID => {
                // TODO
                // let packet = CLogin::read(bytebuf)?;
                // self.entity_id.store(packet.entity_id, Ordering::Relaxed);

                self.send_packet(&SPlayerLoaded).await;
            }
            id if id == CPlayDisconnect::PACKET_ID => {
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

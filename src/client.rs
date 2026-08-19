use crossbeam::atomic::AtomicCell;
use pumpkin_protocol::codec::lp_vector_3d::LpVector3d;
use pumpkin_protocol::codec::var_int::VarInt;
use pumpkin_protocol::java::client::config::{
    CConfigDisconnect, CConfigPing, CFinishConfig, CKnownPacks,
};
use pumpkin_protocol::java::client::login::{
    CEncryptionRequest, CLoginDisconnect, CLoginSuccess, CSetCompression,
};
use pumpkin_protocol::java::client::play::{
    CCombatDeath, CEntityStatus, CEntityVelocity, CKeepAlive, CLogin, CPlayDisconnect,
    CPlayerPosition, CRespawn, CSetHealth,
};
use pumpkin_protocol::java::packet_decoder::TCPNetworkDecoder;
use pumpkin_protocol::java::packet_encoder::TCPNetworkEncoder;
use pumpkin_protocol::java::server::config::{SAcknowledgeFinishConfig, SConfigPong, SKnownPacks};
use pumpkin_protocol::java::server::handshake::SHandShake;
use pumpkin_protocol::java::server::login::{SEncryptionResponse, SLoginAcknowledged, SLoginStart};
use pumpkin_protocol::java::server::play::{
    FLAG_ON_GROUND, SChatMessage, SClientCommand, SConfirmTeleport, SKeepAlive, SPlayerLoaded,
    SPlayerPosition, SPlayerPositionRotation, SPlayerRotation, SSetPlayerGround, SSwingArm,
};
use pumpkin_protocol::packet::MultiVersionJavaPacket;
use pumpkin_protocol::ser::NetworkWriteExt;
use pumpkin_protocol::ser::{NetworkReadExt, ReadingError, WritingError};
use pumpkin_protocol::{
    ClientPacket, CompressionLevel, CompressionThreshold, ConnectionState, PacketDecodeError,
    RawPacket, ServerPacket,
};
use pumpkin_util::math::vector3::Vector3;
use pumpkin_util::version::JavaMinecraftVersion;
use rsa::pkcs8::DecodePublicKey;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};
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

pub const VERSION: JavaMinecraftVersion = JavaMinecraftVersion::V_26_2;

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

    entity_id: AtomicI32,

    message_spam_cooldown: AtomicU32,
    message_count: AtomicU32,
    is_loaded: AtomicBool,
    pub is_dead: AtomicBool,
    swing_cooldown: AtomicU32,

    // Position & Physics
    current_x: AtomicCell<f64>,
    current_y: AtomicCell<f64>,
    current_z: AtomicCell<f64>,
    ground_y: AtomicCell<f64>,
    on_ground: AtomicBool,

    velocity_x: AtomicCell<f64>,
    velocity_y: AtomicCell<f64>,
    velocity_z: AtomicCell<f64>,

    // Last sent position / rotation for delta synchronization
    last_sent_x: AtomicCell<f64>,
    last_sent_y: AtomicCell<f64>,
    last_sent_z: AtomicCell<f64>,
    last_sent_yaw: AtomicCell<f32>,
    last_sent_pitch: AtomicCell<f32>,
    last_sent_on_ground: AtomicBool,
    ticks_since_last_sync: AtomicU32,

    // Movement AI
    is_walking: AtomicBool,
    move_ticks: AtomicU32,
    move_cooldown: AtomicU32,
    move_dir_x: AtomicCell<f64>,
    move_dir_z: AtomicCell<f64>,
    jump_cooldown: AtomicU32,

    // Rotation
    current_yaw: AtomicCell<f32>,
    current_pitch: AtomicCell<f32>,
    start_yaw: AtomicCell<f32>,
    start_pitch: AtomicCell<f32>,
    target_yaw: AtomicCell<f32>,
    target_pitch: AtomicCell<f32>,
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
            closed: AtomicBool::new(false),
            is_loaded: AtomicBool::new(false),
            is_dead: AtomicBool::new(false),
            close_interrupt: Arc::new(Notify::new()),

            message_spam_cooldown: AtomicU32::new(1),
            message_count: AtomicU32::new(0),
            swing_cooldown: AtomicU32::new(0),

            // Rotation
            current_yaw: AtomicCell::new(0.0),
            current_pitch: AtomicCell::new(0.0),
            start_yaw: AtomicCell::new(0.0),
            start_pitch: AtomicCell::new(0.0),
            target_yaw: AtomicCell::new(0.0),
            target_pitch: AtomicCell::new(0.0),
            rotation_progress: AtomicCell::new(1.0),
            rotation_cooldown: AtomicU32::new(0),

            // Position & Physics
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
            last_sent_on_ground: AtomicBool::new(true),
            ticks_since_last_sync: AtomicU32::new(0),

            // Movement AI
            is_walking: AtomicBool::new(false),
            move_ticks: AtomicU32::new(0),
            move_cooldown: AtomicU32::new(0),
            move_dir_x: AtomicCell::new(0.0),
            move_dir_z: AtomicCell::new(0.0),
            jump_cooldown: AtomicU32::new(0),
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

    /// Enables AES-128 CFB8 encryption for the connection using the shared secret.
    pub async fn set_encryption(&self, key: &[u8; 16]) {
        if let Err(err) = self.network_reader.lock().await.set_encryption(key) {
            log::error!("Failed to set decoder encryption: {err}");
        }
        if let Err(err) = self.network_writer.lock().await.set_encryption(key) {
            log::error!("Failed to set encoder encryption: {err}");
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

    pub async fn tick(&self, args: &Args) {
        if self.connection_state.load() != ConnectionState::Play {
            return;
        }
        if !self.is_loaded.load(Ordering::Relaxed) || self.is_dead.load(Ordering::Relaxed) {
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
                self.send_message(spam_message).await;
            }
        }

        if args.enable_swing {
            self.tick_swing().await;
        }

        if args.enable_movement {
            self.tick_movement_ai(args).await;
        }

        if args.enable_rotation {
            self.tick_rotation().await;
        }

        if args.enable_physics {
            self.tick_physics().await;
        }

        self.sync_movement().await;
    }

    async fn tick_movement_ai(&self, args: &Args) {
        let is_walking = self.is_walking.load(Ordering::Relaxed);
        let on_ground = self.on_ground.load(Ordering::Relaxed);

        if is_walking {
            let ticks = self.move_ticks.fetch_sub(1, Ordering::Relaxed);
            let dir_x = self.move_dir_x.load();
            let dir_z = self.move_dir_z.load();

            // Walking acceleration (stronger on ground, slight in air)
            let accel = if on_ground { 0.08 } else { 0.02 };
            let vx = self.velocity_x.load() + dir_x * accel;
            let vz = self.velocity_z.load() + dir_z * accel;
            self.velocity_x.store(vx);
            self.velocity_z.store(vz);

            // Jumping behavior while moving
            if args.enable_jumping {
                let jump_cd = self.jump_cooldown.load(Ordering::Relaxed);
                if jump_cd == 0 && on_ground && rand::random_bool(0.04) {
                    self.velocity_y.store(0.42);
                    self.on_ground.store(false, Ordering::Relaxed);
                    self.jump_cooldown
                        .store(rand::random_range(15..40), Ordering::Relaxed);
                } else if jump_cd > 0 {
                    self.jump_cooldown.fetch_sub(1, Ordering::Relaxed);
                }
            }

            if ticks <= 1 {
                self.is_walking.store(false, Ordering::Relaxed);
                self.move_cooldown
                    .store(rand::random_range(20..80), Ordering::Relaxed);
            }
        } else {
            let cd = self.move_cooldown.load(Ordering::Relaxed);
            if cd == 0 {
                // Pick a new random angle and start walking
                let angle = rand::random_range(-std::f64::consts::PI..std::f64::consts::PI);
                let dir_x = -angle.sin();
                let dir_z = angle.cos();

                self.move_dir_x.store(dir_x);
                self.move_dir_z.store(dir_z);
                self.move_ticks
                    .store(rand::random_range(20..60), Ordering::Relaxed);
                self.is_walking.store(true, Ordering::Relaxed);

                if args.enable_rotation {
                    self.start_yaw.store(self.current_yaw.load());
                    self.start_pitch.store(self.current_pitch.load());
                    self.target_yaw.store(angle.to_degrees() as f32);
                    self.target_pitch.store(rand::random_range(-10.0..10.0));
                    self.rotation_progress.store(0.0);
                }
            } else {
                self.move_cooldown.fetch_sub(1, Ordering::Relaxed);
            }

            let jump_cd = self.jump_cooldown.load(Ordering::Relaxed);
            if jump_cd > 0 {
                self.jump_cooldown.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }

    async fn tick_rotation(&self) {
        let progress = self.rotation_progress.load();
        let cooldown = self.rotation_cooldown.load(Ordering::Relaxed);

        if progress >= 1.0 {
            if cooldown == 0 {
                if !self.is_walking.load(Ordering::Relaxed) {
                    let current_y = self.current_yaw.load();
                    let current_p = self.current_pitch.load();

                    self.start_yaw.store(current_y);
                    self.start_pitch.store(current_p);
                    self.target_yaw.store(rand::random_range(-180.0..180.0));
                    self.target_pitch.store(rand::random_range(-30.0..30.0));

                    self.rotation_progress.store(0.0);
                    self.rotation_cooldown
                        .store(rand::random_range(40..100), Ordering::Relaxed);
                }
            } else {
                self.rotation_cooldown.fetch_sub(1, Ordering::Relaxed);
            }
        } else {
            let new_progress = (progress + 0.05).min(1.0);
            self.rotation_progress.store(new_progress);

            // S-Curve interpolation
            let t = 3.0 * new_progress.powi(2) - 2.0 * new_progress.powi(3);

            let start_y = self.start_yaw.load();
            let target_y = self.target_yaw.load();
            let mut diff_y = (target_y - start_y) % 360.0;
            if diff_y > 180.0 {
                diff_y -= 360.0;
            } else if diff_y < -180.0 {
                diff_y += 360.0;
            }
            let interpolated_yaw = (start_y + diff_y * t) % 360.0;

            let start_p = self.start_pitch.load();
            let target_p = self.target_pitch.load();
            let interpolated_pitch = (start_p + (target_p - start_p) * t).clamp(-90.0, 90.0);

            self.current_yaw.store(interpolated_yaw);
            self.current_pitch.store(interpolated_pitch);
        }
    }

    async fn tick_physics(&self) {
        let mut vx = self.velocity_x.load();
        let mut vy = self.velocity_y.load();
        let mut vz = self.velocity_z.load();

        let cur_x = self.current_x.load();
        let cur_y = self.current_y.load();
        let cur_z = self.current_z.load();
        let ground_y = self.ground_y.load();
        let on_ground = self.on_ground.load(Ordering::Relaxed);

        // 1. Gravity and vertical drag
        if !on_ground {
            vy = (vy - 0.08) * 0.98;
        } else if vy < 0.0 {
            vy = 0.0;
        }

        // 2. Horizontal friction & air resistance
        let friction = if on_ground { 0.6 * 0.91 } else { 0.91 };
        vx *= friction;
        vz *= friction;

        if vx.abs() < 1e-4 {
            vx = 0.0;
        }
        if vz.abs() < 1e-4 {
            vz = 0.0;
        }
        if vy.abs() < 1e-4 && on_ground {
            vy = 0.0;
        }

        // 3. Integrate position
        let new_x = cur_x + vx;
        let mut new_y = cur_y + vy;
        let new_z = cur_z + vz;

        // 4. Ground collision
        let new_on_ground = if new_y <= ground_y {
            new_y = ground_y;
            vy = 0.0;
            true
        } else {
            false
        };

        self.velocity_x.store(vx);
        self.velocity_y.store(vy);
        self.velocity_z.store(vz);

        self.current_x.store(new_x);
        self.current_y.store(new_y);
        self.current_z.store(new_z);
        self.on_ground.store(new_on_ground, Ordering::Relaxed);
    }

    async fn sync_movement(&self) {
        let cur_x = self.current_x.load();
        let cur_y = self.current_y.load();
        let cur_z = self.current_z.load();
        let cur_yaw = self.current_yaw.load();
        let cur_pitch = self.current_pitch.load();
        let cur_on_ground = self.on_ground.load(Ordering::Relaxed);

        let last_x = self.last_sent_x.load();
        let last_y = self.last_sent_y.load();
        let last_z = self.last_sent_z.load();
        let last_yaw = self.last_sent_yaw.load();
        let last_pitch = self.last_sent_pitch.load();
        let last_on_ground = self.last_sent_on_ground.load(Ordering::Relaxed);

        let pos_changed = (cur_x - last_x).abs() > 1e-4
            || (cur_y - last_y).abs() > 1e-4
            || (cur_z - last_z).abs() > 1e-4;

        let rot_changed =
            (cur_yaw - last_yaw).abs() > 0.01 || (cur_pitch - last_pitch).abs() > 0.01;

        let ground_changed = cur_on_ground != last_on_ground;
        let ticks_since_sync = self.ticks_since_last_sync.fetch_add(1, Ordering::Relaxed);

        let collision = if cur_on_ground { FLAG_ON_GROUND } else { 0 };

        if pos_changed && rot_changed {
            self.send_packet(&SPlayerPositionRotation {
                position: Vector3::new(cur_x, cur_y, cur_z),
                yaw: cur_yaw,
                pitch: cur_pitch,
                collision,
            })
            .await;
            self.ticks_since_last_sync.store(0, Ordering::Relaxed);
        } else if pos_changed {
            self.send_packet(&SPlayerPosition {
                position: Vector3::new(cur_x, cur_y, cur_z),
                collision,
            })
            .await;
            self.ticks_since_last_sync.store(0, Ordering::Relaxed);
        } else if rot_changed {
            self.send_packet(&SPlayerRotation {
                yaw: cur_yaw,
                pitch: cur_pitch,
                ground: cur_on_ground,
            })
            .await;
            self.ticks_since_last_sync.store(0, Ordering::Relaxed);
        } else if ground_changed || ticks_since_sync >= 20 {
            self.send_packet(&SSetPlayerGround {
                on_ground: cur_on_ground,
            })
            .await;
            self.ticks_since_last_sync.store(0, Ordering::Relaxed);
        }

        self.last_sent_x.store(cur_x);
        self.last_sent_y.store(cur_y);
        self.last_sent_z.store(cur_z);
        self.last_sent_yaw.store(cur_yaw);
        self.last_sent_pitch.store(cur_pitch);
        self.last_sent_on_ground
            .store(cur_on_ground, Ordering::Relaxed);
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

    pub async fn send_message(&self, message: &str) {
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
            acknowledged: &[0; 3],
            checksum: 0,
        })
        .await;
    }

    pub fn write_packet<P: ClientPacket>(
        packet: &P,
        write: impl Write,
    ) -> Result<(), WritingError> {
        let mut write = write;
        write.write_var_int(&VarInt(P::to_id(VERSION)))?;
        packet.write_packet_data(write, &VERSION)
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
        let mut encoder = self.network_writer.lock().await;
        if let Err(err) = encoder.write_packet(packet_buf.into()).await
            && !self.closed.load(Ordering::Relaxed)
        {
            log::warn!("Failed to send packet to server: {err}");
            // We now need to close the connection to the client since the stream is in an
            // unknown state
            self.close().await;
            return;
        }
        if let Err(err) = encoder.flush().await
            && !self.closed.load(Ordering::Relaxed)
        {
            log::warn!("Failed to flush packet to server: {err}");
            self.close().await;
        }
    }

    pub async fn join_server(&self, address: SocketAddr, name: String) {
        self.send_packet(&SHandShake {
            protocol_version: VarInt(VERSION.protocol_version()),
            server_address: address.ip().to_string().into_boxed_str(),
            server_port: address.port(),
            next_state: pumpkin_protocol::ConnectionState::Login,
        })
        .await;
        self.connection_state.store(ConnectionState::Login);
        self.send_packet(&SLoginStart {
            name: name.into_boxed_str(),
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
        let mut bytebuf = &packet.payload[..];
        match packet.id {
            id if id == CEncryptionRequest::to_id(VERSION) => {
                log::trace!("Handling Encryption Request");
                let packet = CEncryptionRequest::read(&mut bytebuf, &VERSION)?;
                let shared_secret: [u8; 16] = rand::random();

                let public_key =
                    RsaPublicKey::from_public_key_der(packet.public_key).map_err(|e| {
                        ReadingError::Message(format!("Failed to parse RSA public key: {e}"))
                    })?;

                let (encrypted_shared_secret, encrypted_verify_token) = {
                    let mut rng = rand::rng();
                    let enc_secret = public_key
                        .encrypt(&mut rng, Pkcs1v15Encrypt, &shared_secret)
                        .map_err(|e| {
                            ReadingError::Message(format!("Failed to encrypt shared secret: {e}"))
                        })?;
                    let enc_token = public_key
                        .encrypt(&mut rng, Pkcs1v15Encrypt, packet.verify_token)
                        .map_err(|e| {
                            ReadingError::Message(format!("Failed to encrypt verify token: {e}"))
                        })?;
                    (enc_secret, enc_token)
                };

                self.send_packet(&SEncryptionResponse {
                    shared_secret: encrypted_shared_secret.into_boxed_slice(),
                    verify_token: encrypted_verify_token.into_boxed_slice(),
                })
                .await;

                self.set_encryption(&shared_secret).await;
                log::trace!("Encryption enabled successfully");
            }
            id if id == CSetCompression::to_id(VERSION) => {
                log::trace!("Set Compression");
                let packet = CSetCompression::read(&mut bytebuf, &VERSION)?;
                self.set_compression(Some((packet.threshold.0 as usize, 6)))
                    .await
            }
            id if id == CLoginDisconnect::to_id(VERSION) => {
                log::error!("Kicking in Login State");
                self.close().await;
            }
            id if id == CLoginSuccess::to_id(VERSION) => {
                log::trace!("Login -> Config");
                self.send_packet(&SLoginAcknowledged).await;
                self.connection_state.store(ConnectionState::Config);
                log::trace!("Sending Known packs");
                self.send_packet(&SKnownPacks {
                    known_packs: Vec::new(),
                })
                .await;
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_config_packet(&self, packet: &mut RawPacket) -> Result<(), ReadingError> {
        let mut bytebuf = &packet.payload[..];
        match packet.id {
            id if id == CConfigDisconnect::to_id(VERSION) => {
                log::error!("Kicking in Config State");
                self.close().await;
            }
            id if id == CKnownPacks::to_id(VERSION) => {
                log::trace!("Received CKnownPacks, sending SKnownPacks");
                self.send_packet(&SKnownPacks {
                    known_packs: Vec::new(),
                })
                .await;
            }
            id if id == CConfigPing::to_id(VERSION) => {
                let ping = CConfigPing::read(&mut bytebuf, &VERSION)?;
                self.send_packet(&SConfigPong { id: ping.id }).await;
            }
            id if id == CFinishConfig::to_id(VERSION) => {
                log::trace!("Config -> Play");
                self.send_packet(&SAcknowledgeFinishConfig).await;
                self.connection_state.store(ConnectionState::Play);
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_play_packet(&self, packet: &mut RawPacket) -> Result<(), ReadingError> {
        let mut bytebuf = &packet.payload[..];
        match packet.id {
            id if id == CKeepAlive::to_id(VERSION) => {
                let packet = CKeepAlive::read(&mut bytebuf, &VERSION)?;
                self.send_packet(&SKeepAlive {
                    keep_alive_id: packet.keep_alive_id,
                })
                .await;
            }
            id if id == CEntityVelocity::to_id(VERSION) => {
                let entity_id = bytebuf.get_var_int()?;
                if entity_id.0 == self.entity_id.load(Ordering::Relaxed) {
                    let velocity = LpVector3d::read(&mut bytebuf)?.0;
                    self.velocity_x.store(velocity.x);
                    self.velocity_y.store(velocity.y);
                    self.velocity_z.store(velocity.z);
                    if velocity.y > 0.0 {
                        self.on_ground.store(false, Ordering::Relaxed);
                    }
                }
            }
            id if id == CPlayerPosition::to_id(VERSION) => {
                let packet = CPlayerPosition::read(&mut bytebuf, &VERSION)?;
                self.current_yaw.store(packet.yaw);
                self.current_pitch.store(packet.pitch);
                self.target_yaw.store(packet.yaw);
                self.target_pitch.store(packet.pitch);
                self.rotation_progress.store(1.0);

                let x = packet.position.x;
                let y = packet.position.y;
                let z = packet.position.z;

                self.current_x.store(x);
                self.current_y.store(y);
                self.current_z.store(z);
                self.ground_y.store(y);

                self.velocity_x.store(packet.delta.x);
                self.velocity_y.store(packet.delta.y);
                self.velocity_z.store(packet.delta.z);

                let on_ground = packet.delta.y <= 0.0;
                self.on_ground.store(on_ground, Ordering::Relaxed);

                self.last_sent_x.store(x);
                self.last_sent_y.store(y);
                self.last_sent_z.store(z);
                self.last_sent_yaw.store(packet.yaw);
                self.last_sent_pitch.store(packet.pitch);
                self.last_sent_on_ground.store(on_ground, Ordering::Relaxed);

                self.is_walking.store(false, Ordering::Relaxed);
                self.move_cooldown
                    .store(rand::random_range(20..60), Ordering::Relaxed);

                self.send_packet(&SConfirmTeleport {
                    teleport_id: packet.teleport_id,
                })
                .await;
                self.is_loaded.store(true, Ordering::Relaxed);
                self.is_dead.store(false, Ordering::Relaxed);
            }
            id if id == CCombatDeath::to_id(VERSION) => {
                let player_id = bytebuf.get_var_int()?;
                if player_id.0 == self.entity_id.load(Ordering::Relaxed) {
                    self.respawn().await;
                }
            }
            id if id == CSetHealth::to_id(VERSION) => {
                let health = bytebuf.get_f32()?;
                if health <= 0.0 {
                    self.respawn().await;
                } else {
                    self.is_dead.store(false, Ordering::Relaxed);
                }
            }
            id if id == CEntityStatus::to_id(VERSION) => {
                let entity_id = bytebuf.get_i32_be()?;
                let entity_status = bytebuf.get_i8()?;
                if entity_id == self.entity_id.load(Ordering::Relaxed) && entity_status == 3 {
                    self.respawn().await;
                }
            }
            id if id == CRespawn::to_id(VERSION) => {
                self.send_packet(&SPlayerLoaded).await;
            }
            id if id == CLogin::to_id(VERSION) => {
                let entity_id = bytebuf.get_i32_be()?;
                self.entity_id.store(entity_id, Ordering::Relaxed);
                self.send_packet(&SPlayerLoaded).await;
            }
            id if id == CPlayDisconnect::to_id(VERSION) => {
                log::error!("Kicking in Play State");
                self.close().await;
            }
            _ => {}
        }
        Ok(())
    }

    pub async fn respawn(&self) {
        if self
            .is_dead
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            self.is_loaded.store(false, Ordering::Relaxed);
            self.is_walking.store(false, Ordering::Relaxed);
            self.velocity_x.store(0.0);
            self.velocity_y.store(0.0);
            self.velocity_z.store(0.0);

            self.send_packet(&SClientCommand {
                action_id: VarInt(0),
            })
            .await;
        }
    }

    pub async fn close(&self) {
        self.close_interrupt.notify_waiters();
        self.closed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn create_test_client() -> Client {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connect_handle = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (server_stream, _) = listener.accept().await.unwrap();
        let _ = server_stream;
        let client_stream = connect_handle.await.unwrap();
        Client::new(client_stream)
    }

    #[tokio::test]
    async fn test_gravity_and_ground_collision() {
        let client = create_test_client().await;
        client.current_y.store(10.0);
        client.ground_y.store(0.0);
        client.on_ground.store(false, Ordering::Relaxed);

        // Tick physics multiple times in the air
        for _ in 0..10 {
            client.tick_physics().await;
        }

        // The bot should have fallen down and velocity.y should be negative
        assert!(client.current_y.load() < 10.0);
        assert!(client.velocity_y.load() < 0.0);

        // Tick until it lands
        for _ in 0..50 {
            client.tick_physics().await;
        }

        assert_eq!(client.current_y.load(), 0.0);
        assert_eq!(client.velocity_y.load(), 0.0);
        assert!(client.on_ground.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_jump_physics() {
        let client = create_test_client().await;
        client.current_y.store(0.0);
        client.ground_y.store(0.0);
        client.on_ground.store(true, Ordering::Relaxed);

        // Apply jump impulse
        client.velocity_y.store(0.42);
        client.on_ground.store(false, Ordering::Relaxed);

        client.tick_physics().await;
        assert!(client.current_y.load() > 0.0);
        assert!(!client.on_ground.load(Ordering::Relaxed));

        // Tick until landing back on ground
        for _ in 0..50 {
            client.tick_physics().await;
        }

        assert_eq!(client.current_y.load(), 0.0);
        assert!(client.on_ground.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_horizontal_friction_decay() {
        let client = create_test_client().await;
        client.current_y.store(0.0);
        client.ground_y.store(0.0);
        client.on_ground.store(true, Ordering::Relaxed);

        client.velocity_x.store(1.0);
        client.tick_physics().await;

        let vx = client.velocity_x.load();
        assert!(vx < 1.0);
        assert!((vx - 0.546).abs() < 1e-3);
    }

    #[tokio::test]
    async fn test_encryption_handshake_and_encrypted_stream() {
        use rsa::RsaPrivateKey;
        use rsa::pkcs8::EncodePublicKey;

        let mut rng = rand::rng();
        let server_private_key = RsaPrivateKey::new(&mut rng, 1024).unwrap();
        let server_public_key_der = server_private_key
            .to_public_key()
            .to_public_key_der()
            .unwrap()
            .into_vec();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let connect_handle = tokio::spawn(async move {
            let stream = TcpStream::connect(addr).await.unwrap();
            let client = Arc::new(Client::new(stream));
            client.connection_state.store(ConnectionState::Login);
            client
        });

        let (server_stream, _) = listener.accept().await.unwrap();
        let client = connect_handle.await.unwrap();

        let (server_reader, server_writer) = server_stream.into_split();
        let mut server_decoder = TCPNetworkDecoder::new(BufReader::new(server_reader));
        let mut server_encoder = TCPNetworkEncoder::new(BufWriter::new(server_writer));

        // 1. Server sends CEncryptionRequest
        let verify_token = [1u8, 2, 3, 4];
        let enc_req = CEncryptionRequest::new("", &server_public_key_der, &verify_token, false);
        let mut enc_req_buf = Vec::new();
        Client::write_packet(&enc_req, &mut enc_req_buf).unwrap();
        server_encoder
            .write_packet(enc_req_buf.into())
            .await
            .unwrap();
        server_encoder.flush().await.unwrap();

        // 2. Client processes the packet (CEncryptionRequest -> SEncryptionResponse + enables encryption)
        let client_clone = client.clone();
        let client_process_handle =
            tokio::spawn(async move { client_clone.process_packets().await });

        // 3. Server receives SEncryptionResponse (unencrypted)
        let raw_resp = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_resp.id, SEncryptionResponse::to_id(VERSION));
        let mut resp_payload = &raw_resp.payload[..];
        let resp = SEncryptionResponse::read(&mut resp_payload, &VERSION).unwrap();

        // 4. Server decrypts shared secret and verify token
        let decrypted_secret = server_private_key
            .decrypt(Pkcs1v15Encrypt, &resp.shared_secret)
            .unwrap();
        let decrypted_token = server_private_key
            .decrypt(Pkcs1v15Encrypt, &resp.verify_token)
            .unwrap();

        assert_eq!(decrypted_token.as_slice(), &verify_token);
        assert_eq!(decrypted_secret.len(), 16);

        let shared_secret: [u8; 16] = decrypted_secret.try_into().unwrap();

        // 5. Server enables encryption
        server_decoder.set_encryption(&shared_secret).unwrap();
        server_encoder.set_encryption(&shared_secret).unwrap();

        assert!(client_process_handle.await.unwrap());

        // 6. Test encrypted communication: Server sends encrypted CKeepAlive
        let keep_alive = CKeepAlive {
            keep_alive_id: 123456,
        };
        let mut ka_buf = Vec::new();
        Client::write_packet(&keep_alive, &mut ka_buf).unwrap();
        server_encoder.write_packet(ka_buf.into()).await.unwrap();
        server_encoder.flush().await.unwrap();

        // 7. Client receives and handles encrypted CKeepAlive, sends encrypted SKeepAlive back
        client.connection_state.store(ConnectionState::Play);
        let client_clone = client.clone();
        let client_process_handle =
            tokio::spawn(async move { client_clone.process_packets().await });

        // 8. Server reads encrypted SKeepAlive from client
        let raw_ka_resp = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_ka_resp.id, SKeepAlive::to_id(VERSION));
        let mut ka_resp_payload = &raw_ka_resp.payload[..];
        let ka_resp = SKeepAlive::read(&mut ka_resp_payload, &VERSION).unwrap();
        assert_eq!(ka_resp.keep_alive_id, 123456);

        assert!(client_process_handle.await.unwrap());
    }

    #[tokio::test]
    async fn test_chat_message_with_encryption_and_compression() {
        use pumpkin_protocol::java::client::play::CSystemChatMessage;
        use pumpkin_util::text::TextComponent;
        use rsa::RsaPrivateKey;
        use rsa::pkcs8::EncodePublicKey;

        let mut rng = rand::rng();
        let server_private_key = RsaPrivateKey::new(&mut rng, 1024).unwrap();
        let server_public_key_der = server_private_key
            .to_public_key()
            .to_public_key_der()
            .unwrap()
            .into_vec();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let connect_handle = tokio::spawn(async move {
            let stream = TcpStream::connect(addr).await.unwrap();
            let client = Arc::new(Client::new(stream));
            client.connection_state.store(ConnectionState::Login);
            client
        });

        let (server_stream, _) = listener.accept().await.unwrap();
        let client = connect_handle.await.unwrap();

        let (server_reader, server_writer) = server_stream.into_split();
        let mut server_decoder = TCPNetworkDecoder::new(BufReader::new(server_reader));
        let mut server_encoder = TCPNetworkEncoder::new(BufWriter::new(server_writer));

        // 1. Server sends CSetCompression (threshold 256)
        let set_comp = CSetCompression {
            threshold: VarInt(256),
        };
        let mut set_comp_buf = Vec::new();
        Client::write_packet(&set_comp, &mut set_comp_buf).unwrap();
        server_encoder
            .write_packet(set_comp_buf.into())
            .await
            .unwrap();
        server_encoder.flush().await.unwrap();

        let client_clone = client.clone();
        let handle = tokio::spawn(async move { client_clone.process_packets().await });
        server_decoder.set_compression(256);
        server_encoder.set_compression((256, 4));
        assert!(handle.await.unwrap());

        // 2. Server sends CEncryptionRequest
        let verify_token = [1u8, 2, 3, 4];
        let enc_req = CEncryptionRequest::new("", &server_public_key_der, &verify_token, false);
        let mut enc_req_buf = Vec::new();
        Client::write_packet(&enc_req, &mut enc_req_buf).unwrap();
        server_encoder
            .write_packet(enc_req_buf.into())
            .await
            .unwrap();
        server_encoder.flush().await.unwrap();

        let client_clone = client.clone();
        let handle = tokio::spawn(async move { client_clone.process_packets().await });

        let raw_resp = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_resp.id, SEncryptionResponse::to_id(VERSION));
        let mut resp_payload = &raw_resp.payload[..];
        let resp = SEncryptionResponse::read(&mut resp_payload, &VERSION).unwrap();

        let decrypted_secret = server_private_key
            .decrypt(Pkcs1v15Encrypt, &resp.shared_secret)
            .unwrap();
        let shared_secret: [u8; 16] = decrypted_secret.try_into().unwrap();
        server_decoder.set_encryption(&shared_secret).unwrap();
        server_encoder.set_encryption(&shared_secret).unwrap();
        assert!(handle.await.unwrap());

        // 3. Move to play state
        client.connection_state.store(ConnectionState::Play);

        // 4. Client sends SChatMessage
        client.send_message("Hello from bot!").await;

        // 5. Server reads SChatMessage
        let raw_chat = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_chat.id, SChatMessage::to_id(VERSION));
        let mut chat_payload = &raw_chat.payload[..];
        let chat = SChatMessage::read(&mut chat_payload, &VERSION).unwrap();
        assert_eq!(chat.message, "Hello from bot!");

        // 6. Server broadcasts CSystemChatMessage (< 256 bytes uncompressed)
        let text_comp = TextComponent::text("<BOT_0> Hello from bot!");
        let sys_chat = CSystemChatMessage::new(&text_comp, false);
        let mut chat_buf = Vec::new();
        Client::write_packet(&sys_chat, &mut chat_buf).unwrap();
        server_encoder.write_packet(chat_buf.into()).await.unwrap();
        server_encoder.flush().await.unwrap();

        // 7. Client receives and processes CSystemChatMessage
        let client_clone = client.clone();
        let handle = tokio::spawn(async move { client_clone.process_packets().await });
        assert!(handle.await.unwrap());

        // 8. Server sends CKeepAlive
        let keep_alive = CKeepAlive {
            keep_alive_id: 999999,
        };
        let mut ka_buf = Vec::new();
        Client::write_packet(&keep_alive, &mut ka_buf).unwrap();
        server_encoder.write_packet(ka_buf.into()).await.unwrap();
        server_encoder.flush().await.unwrap();

        // 9. Client receives and processes CKeepAlive, responds with SKeepAlive
        let client_clone = client.clone();
        let handle = tokio::spawn(async move { client_clone.process_packets().await });

        let raw_ka_resp = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_ka_resp.id, SKeepAlive::to_id(VERSION));
        let mut ka_resp_payload = &raw_ka_resp.payload[..];
        let ka_resp = SKeepAlive::read(&mut ka_resp_payload, &VERSION).unwrap();
        assert_eq!(ka_resp.keep_alive_id, 999999);
        assert!(handle.await.unwrap());
    }

    #[tokio::test]
    async fn test_multiple_compressed_packets_streaming() {
        use pumpkin_protocol::java::client::play::CSystemChatMessage;
        use pumpkin_util::text::TextComponent;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let connect_handle = tokio::spawn(async move {
            let stream = TcpStream::connect(addr).await.unwrap();
            let client = Arc::new(Client::new(stream));
            client.connection_state.store(ConnectionState::Play);
            client
        });

        let (server_stream, _) = listener.accept().await.unwrap();
        let client = connect_handle.await.unwrap();

        let (server_reader, server_writer) = server_stream.into_split();
        let mut server_decoder = TCPNetworkDecoder::new(BufReader::new(server_reader));
        let mut server_encoder = TCPNetworkEncoder::new(BufWriter::new(server_writer));

        // Enable compression on both client and server
        client.set_compression(Some((256, 4))).await;
        server_decoder.set_compression(256);
        server_encoder.set_compression((256, 4));

        // 1. Send a large packet (> 256 bytes, compressed)
        let large_msg = "A".repeat(1000);
        let text_comp = TextComponent::text(large_msg);
        let sys_chat = CSystemChatMessage::new(&text_comp, false);
        let mut chat_buf = Vec::new();
        Client::write_packet(&sys_chat, &mut chat_buf).unwrap();
        server_encoder.write_packet(chat_buf.into()).await.unwrap();

        // 2. Immediately send a small packet (< 256 bytes, uncompressed)
        let keep_alive = CKeepAlive {
            keep_alive_id: 111222,
        };
        let mut ka_buf = Vec::new();
        Client::write_packet(&keep_alive, &mut ka_buf).unwrap();
        server_encoder.write_packet(ka_buf.into()).await.unwrap();
        server_encoder.flush().await.unwrap();

        // 3. Client receives large packet
        let client_clone = client.clone();
        let handle1 = tokio::spawn(async move { client_clone.process_packets().await });
        assert!(handle1.await.unwrap());

        // 4. Client receives keep_alive packet
        let client_clone = client.clone();
        let handle2 = tokio::spawn(async move { client_clone.process_packets().await });

        let raw_ka_resp = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_ka_resp.id, SKeepAlive::to_id(VERSION));
        let mut ka_resp_payload = &raw_ka_resp.payload[..];
        let ka_resp = SKeepAlive::read(&mut ka_resp_payload, &VERSION).unwrap();
        assert_eq!(ka_resp.keep_alive_id, 111222);
        assert!(handle2.await.unwrap());
    }

    #[tokio::test]
    async fn test_respawn_on_combat_death() {
        use pumpkin_util::text::TextComponent;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let connect_handle = tokio::spawn(async move {
            let stream = TcpStream::connect(addr).await.unwrap();
            let client = Arc::new(Client::new(stream));
            client.connection_state.store(ConnectionState::Play);
            client.entity_id.store(42, Ordering::Relaxed);
            client.is_loaded.store(true, Ordering::Relaxed);
            client
        });

        let (server_stream, _) = listener.accept().await.unwrap();
        let client = connect_handle.await.unwrap();

        let (server_reader, server_writer) = server_stream.into_split();
        let mut server_decoder = TCPNetworkDecoder::new(BufReader::new(server_reader));
        let mut server_encoder = TCPNetworkEncoder::new(BufWriter::new(server_writer));

        // Server sends CCombatDeath for entity 42
        let death_msg = TextComponent::text("Bot died");
        let combat_death = CCombatDeath::new(VarInt(42), &death_msg);
        let mut death_buf = Vec::new();
        Client::write_packet(&combat_death, &mut death_buf).unwrap();
        server_encoder.write_packet(death_buf.into()).await.unwrap();
        server_encoder.flush().await.unwrap();

        let client_clone = client.clone();
        let handle = tokio::spawn(async move { client_clone.process_packets().await });

        // Server receives SClientCommand with action_id 0 (respawn)
        let raw_cmd = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_cmd.id, SClientCommand::to_id(VERSION));
        let mut payload = &raw_cmd.payload[..];
        let client_cmd = SClientCommand::read(&mut payload, &VERSION).unwrap();
        assert_eq!(client_cmd.action_id.0, 0);

        assert!(handle.await.unwrap());
        assert!(client.is_dead.load(Ordering::Relaxed));
        assert!(!client.is_loaded.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_respawn_on_set_health_zero() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let connect_handle = tokio::spawn(async move {
            let stream = TcpStream::connect(addr).await.unwrap();
            let client = Arc::new(Client::new(stream));
            client.connection_state.store(ConnectionState::Play);
            client.is_loaded.store(true, Ordering::Relaxed);
            client
        });

        let (server_stream, _) = listener.accept().await.unwrap();
        let client = connect_handle.await.unwrap();

        let (server_reader, server_writer) = server_stream.into_split();
        let mut server_decoder = TCPNetworkDecoder::new(BufReader::new(server_reader));
        let mut server_encoder = TCPNetworkEncoder::new(BufWriter::new(server_writer));

        // Server sends CSetHealth with health 0.0
        let set_health = CSetHealth::new(0.0, VarInt(20), 5.0);
        let mut health_buf = Vec::new();
        Client::write_packet(&set_health, &mut health_buf).unwrap();
        server_encoder
            .write_packet(health_buf.into())
            .await
            .unwrap();
        server_encoder.flush().await.unwrap();

        let client_clone = client.clone();
        let handle = tokio::spawn(async move { client_clone.process_packets().await });

        let raw_cmd = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_cmd.id, SClientCommand::to_id(VERSION));
        let mut payload = &raw_cmd.payload[..];
        let client_cmd = SClientCommand::read(&mut payload, &VERSION).unwrap();
        assert_eq!(client_cmd.action_id.0, 0);

        assert!(handle.await.unwrap());
        assert!(client.is_dead.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_respawn_on_entity_status_death() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let connect_handle = tokio::spawn(async move {
            let stream = TcpStream::connect(addr).await.unwrap();
            let client = Arc::new(Client::new(stream));
            client.connection_state.store(ConnectionState::Play);
            client.entity_id.store(100, Ordering::Relaxed);
            client.is_loaded.store(true, Ordering::Relaxed);
            client
        });

        let (server_stream, _) = listener.accept().await.unwrap();
        let client = connect_handle.await.unwrap();

        let (server_reader, server_writer) = server_stream.into_split();
        let mut server_decoder = TCPNetworkDecoder::new(BufReader::new(server_reader));
        let mut server_encoder = TCPNetworkEncoder::new(BufWriter::new(server_writer));

        // Server sends CEntityStatus with entity_id 100, status 3 (death)
        let entity_status = CEntityStatus::new(100, 3);
        let mut status_buf = Vec::new();
        Client::write_packet(&entity_status, &mut status_buf).unwrap();
        server_encoder
            .write_packet(status_buf.into())
            .await
            .unwrap();
        server_encoder.flush().await.unwrap();

        let client_clone = client.clone();
        let handle = tokio::spawn(async move { client_clone.process_packets().await });

        let raw_cmd = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_cmd.id, SClientCommand::to_id(VERSION));
        let mut payload = &raw_cmd.payload[..];
        let client_cmd = SClientCommand::read(&mut payload, &VERSION).unwrap();
        assert_eq!(client_cmd.action_id.0, 0);

        assert!(handle.await.unwrap());
        assert!(client.is_dead.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_respawn_cycle_and_post_respawn_teleport() {
        use pumpkin_data::dimension::Dimension;
        use pumpkin_protocol::java::client::play::PlayerSpawnData;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let connect_handle = tokio::spawn(async move {
            let stream = TcpStream::connect(addr).await.unwrap();
            let client = Arc::new(Client::new(stream));
            client.connection_state.store(ConnectionState::Play);
            client.entity_id.store(7, Ordering::Relaxed);
            client.is_loaded.store(true, Ordering::Relaxed);
            client
        });

        let (server_stream, _) = listener.accept().await.unwrap();
        let client = connect_handle.await.unwrap();

        let (server_reader, server_writer) = server_stream.into_split();
        let mut server_decoder = TCPNetworkDecoder::new(BufReader::new(server_reader));
        let mut server_encoder = TCPNetworkEncoder::new(BufWriter::new(server_writer));

        // 1. Die via CSetHealth(0.0)
        let set_health = CSetHealth::new(0.0, VarInt(20), 5.0);
        let mut buf = Vec::new();
        Client::write_packet(&set_health, &mut buf).unwrap();
        server_encoder.write_packet(buf.into()).await.unwrap();
        server_encoder.flush().await.unwrap();

        let client_clone = client.clone();
        let handle1 = tokio::spawn(async move { client_clone.process_packets().await });

        // Server receives SClientCommand(0)
        let raw_cmd = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_cmd.id, SClientCommand::to_id(VERSION));
        assert!(handle1.await.unwrap());
        assert!(client.is_dead.load(Ordering::Relaxed));

        // 2. Server sends CRespawn
        let spawn_data = PlayerSpawnData::new(
            Dimension::OVERWORLD.clone(),
            0,
            0,
            -1,
            false,
            false,
            None,
            VarInt(0),
            VarInt(63),
        );
        let respawn = CRespawn::new(spawn_data, 0);
        let mut buf = Vec::new();
        Client::write_packet(&respawn, &mut buf).unwrap();
        server_encoder.write_packet(buf.into()).await.unwrap();
        server_encoder.flush().await.unwrap();

        let client_clone = client.clone();
        let handle2 = tokio::spawn(async move { client_clone.process_packets().await });

        // Server receives SPlayerLoaded
        let raw_loaded = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_loaded.id, SPlayerLoaded::to_id(VERSION));
        assert!(handle2.await.unwrap());

        // 3. Server sends CPlayerPosition (teleport)
        let player_pos = CPlayerPosition::new(
            VarInt(55),
            Vector3::new(10.0, 64.0, 20.0),
            Vector3::new(0.0, 0.0, 0.0),
            90.0,
            0.0,
            Vec::new(),
        );
        let mut buf = Vec::new();
        Client::write_packet(&player_pos, &mut buf).unwrap();
        server_encoder.write_packet(buf.into()).await.unwrap();
        server_encoder.flush().await.unwrap();

        let client_clone = client.clone();
        let handle3 = tokio::spawn(async move { client_clone.process_packets().await });

        // Server receives SConfirmTeleport
        let raw_confirm = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_confirm.id, SConfirmTeleport::to_id(VERSION));
        let mut confirm_payload = &raw_confirm.payload[..];
        let confirm = SConfirmTeleport::read(&mut confirm_payload, &VERSION).unwrap();
        assert_eq!(confirm.teleport_id.0, 55);
        assert!(handle3.await.unwrap());

        // Bot state should now be alive and loaded
        assert!(!client.is_dead.load(Ordering::Relaxed));
        assert!(client.is_loaded.load(Ordering::Relaxed));
        assert_eq!(client.current_x.load(), 10.0);
        assert_eq!(client.current_y.load(), 64.0);
        assert_eq!(client.current_z.load(), 20.0);
    }

    #[tokio::test]
    async fn test_full_join_handshake_flow_with_encryption() {
        use pumpkin_data::dimension::Dimension;
        use pumpkin_protocol::java::client::play::PlayerSpawnData;
        use rsa::RsaPrivateKey;
        use rsa::pkcs8::EncodePublicKey;

        let mut rng = rand::rng();
        let server_private_key = RsaPrivateKey::new(&mut rng, 1024).unwrap();
        let server_public_key_der = server_private_key
            .to_public_key()
            .to_public_key_der()
            .unwrap()
            .into_vec();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let connect_handle = tokio::spawn(async move {
            let stream = TcpStream::connect(addr).await.unwrap();
            let client = Arc::new(Client::new(stream));
            client.join_server(addr, "BOT_1".to_string()).await;
            client
        });

        let (server_stream, _) = listener.accept().await.unwrap();
        let client = connect_handle.await.unwrap();

        let (server_reader, server_writer) = server_stream.into_split();
        let mut server_decoder = TCPNetworkDecoder::new(BufReader::new(server_reader));
        let mut server_encoder = TCPNetworkEncoder::new(BufWriter::new(server_writer));

        // Start background packet processor for client
        let client_proc = client.clone();
        let proc_handle = tokio::spawn(async move {
            while !client_proc.closed.load(Ordering::Relaxed) {
                if !client_proc.process_packets().await {
                    break;
                }
            }
        });

        // 1. Server receives SHandShake
        let raw_handshake = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_handshake.id, 0); // SHandShake id is 0
        let mut hs_payload = &raw_handshake.payload[..];
        let handshake = SHandShake::read(&mut hs_payload, &VERSION).unwrap();
        assert_eq!(handshake.next_state, ConnectionState::Login);

        // 2. Server receives SLoginStart
        let raw_login_start = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_login_start.id, SLoginStart::to_id(VERSION));
        let mut ls_payload = &raw_login_start.payload[..];
        let login_start = SLoginStart::read(&mut ls_payload, &VERSION).unwrap();
        assert_eq!(login_start.name.as_ref(), "BOT_1");

        // 3. Server sends CEncryptionRequest
        let verify_token = [9u8, 8, 7, 6];
        let enc_req = CEncryptionRequest::new("", &server_public_key_der, &verify_token, false);
        let mut enc_req_buf = Vec::new();
        Client::write_packet(&enc_req, &mut enc_req_buf).unwrap();
        server_encoder
            .write_packet(enc_req_buf.into())
            .await
            .unwrap();
        server_encoder.flush().await.unwrap();

        // 4. Server receives SEncryptionResponse (unencrypted)
        let raw_enc_resp = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_enc_resp.id, SEncryptionResponse::to_id(VERSION));
        let mut enc_resp_payload = &raw_enc_resp.payload[..];
        let enc_resp = SEncryptionResponse::read(&mut enc_resp_payload, &VERSION).unwrap();

        // Decrypt shared secret and verify token
        let decrypted_secret = server_private_key
            .decrypt(Pkcs1v15Encrypt, &enc_resp.shared_secret)
            .unwrap();
        let decrypted_token = server_private_key
            .decrypt(Pkcs1v15Encrypt, &enc_resp.verify_token)
            .unwrap();
        assert_eq!(decrypted_token.as_slice(), &verify_token);
        let shared_secret: [u8; 16] = decrypted_secret.try_into().unwrap();

        // Enable encryption on server
        server_decoder.set_encryption(&shared_secret).unwrap();
        server_encoder.set_encryption(&shared_secret).unwrap();

        // 5. Server sends encrypted CLoginSuccess
        let bot_uuid = Uuid::new_v4();
        let login_success = CLoginSuccess::new(&bot_uuid, "BOT_1", &[], false, Uuid::new_v4());
        let mut ls_buf = Vec::new();
        Client::write_packet(&login_success, &mut ls_buf).unwrap();
        server_encoder.write_packet(ls_buf.into()).await.unwrap();
        server_encoder.flush().await.unwrap();

        // 6. Server receives encrypted SLoginAcknowledged
        let raw_login_ack = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_login_ack.id, SLoginAcknowledged::to_id(VERSION));

        // 7. Server receives encrypted SKnownPacks
        let raw_known_packs = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_known_packs.id, SKnownPacks::to_id(VERSION));

        // 8. Server sends encrypted CFinishConfig
        let finish_config = CFinishConfig;
        let mut fc_buf = Vec::new();
        Client::write_packet(&finish_config, &mut fc_buf).unwrap();
        server_encoder.write_packet(fc_buf.into()).await.unwrap();
        server_encoder.flush().await.unwrap();

        // 9. Server receives encrypted SAcknowledgeFinishConfig
        let raw_ack_finish = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_ack_finish.id, SAcknowledgeFinishConfig::to_id(VERSION));

        // 10. Server sends encrypted CLogin and CPlayerPosition
        let spawn_data = PlayerSpawnData::new(
            Dimension::OVERWORLD.clone(),
            0,
            0,
            -1,
            false,
            false,
            None,
            VarInt(0),
            VarInt(63),
        );
        let login_packet = CLogin::new(
            123,
            false,
            &[],
            VarInt(20),
            VarInt(10),
            VarInt(10),
            false,
            true,
            false,
            spawn_data,
            false,
            false,
        );
        let mut login_buf = Vec::new();
        Client::write_packet(&login_packet, &mut login_buf).unwrap();
        server_encoder.write_packet(login_buf.into()).await.unwrap();

        let player_pos = CPlayerPosition::new(
            VarInt(101),
            Vector3::new(50.0, 70.0, -100.0),
            Vector3::new(0.0, 0.0, 0.0),
            45.0,
            0.0,
            Vec::new(),
        );
        let mut pos_buf = Vec::new();
        Client::write_packet(&player_pos, &mut pos_buf).unwrap();
        server_encoder.write_packet(pos_buf.into()).await.unwrap();
        server_encoder.flush().await.unwrap();

        // 11. Server receives encrypted SPlayerLoaded
        let raw_loaded = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_loaded.id, SPlayerLoaded::to_id(VERSION));

        // 12. Server receives encrypted SConfirmTeleport
        let raw_teleport_confirm = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_teleport_confirm.id, SConfirmTeleport::to_id(VERSION));
        let mut conf_payload = &raw_teleport_confirm.payload[..];
        let confirm = SConfirmTeleport::read(&mut conf_payload, &VERSION).unwrap();
        assert_eq!(confirm.teleport_id.0, 101);

        // Verify bot state
        assert_eq!(client.connection_state.load(), ConnectionState::Play);
        assert!(client.is_loaded.load(Ordering::Relaxed));
        assert!(!client.is_dead.load(Ordering::Relaxed));
        assert_eq!(client.current_x.load(), 50.0);
        assert_eq!(client.current_y.load(), 70.0);
        assert_eq!(client.current_z.load(), -100.0);

        // 13. Test bot actions in play state: Bot sends encrypted message
        client.send_message("Bot connected successfully!").await;

        let raw_chat = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_chat.id, SChatMessage::to_id(VERSION));
        let mut chat_payload = &raw_chat.payload[..];
        let chat = SChatMessage::read(&mut chat_payload, &VERSION).unwrap();
        assert_eq!(chat.message, "Bot connected successfully!");

        client.close().await;
        let _ = proc_handle.await;
    }

    #[tokio::test]
    async fn test_full_join_handshake_flow_with_encryption_and_compression() {
        use pumpkin_data::dimension::Dimension;
        use pumpkin_protocol::java::client::play::PlayerSpawnData;
        use rsa::RsaPrivateKey;
        use rsa::pkcs8::EncodePublicKey;

        let mut rng = rand::rng();
        let server_private_key = RsaPrivateKey::new(&mut rng, 1024).unwrap();
        let server_public_key_der = server_private_key
            .to_public_key()
            .to_public_key_der()
            .unwrap()
            .into_vec();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let connect_handle = tokio::spawn(async move {
            let stream = TcpStream::connect(addr).await.unwrap();
            let client = Arc::new(Client::new(stream));
            client.join_server(addr, "BOT_COMPRESSED".to_string()).await;
            client
        });

        let (server_stream, _) = listener.accept().await.unwrap();
        let client = connect_handle.await.unwrap();

        let (server_reader, server_writer) = server_stream.into_split();
        let mut server_decoder = TCPNetworkDecoder::new(BufReader::new(server_reader));
        let mut server_encoder = TCPNetworkEncoder::new(BufWriter::new(server_writer));

        // Start background packet processor for client
        let client_proc = client.clone();
        let proc_handle = tokio::spawn(async move {
            while !client_proc.closed.load(Ordering::Relaxed) {
                if !client_proc.process_packets().await {
                    break;
                }
            }
        });

        // 1. Server receives SHandShake & SLoginStart
        let raw_handshake = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_handshake.id, 0);

        let raw_login_start = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_login_start.id, SLoginStart::to_id(VERSION));

        // 2. Server sends CSetCompression (threshold 256)
        let set_comp = CSetCompression {
            threshold: VarInt(256),
        };
        let mut comp_buf = Vec::new();
        Client::write_packet(&set_comp, &mut comp_buf).unwrap();
        server_encoder.write_packet(comp_buf.into()).await.unwrap();
        server_encoder.flush().await.unwrap();

        server_decoder.set_compression(256);
        server_encoder.set_compression((256, 6));

        // 3. Server sends CEncryptionRequest (now compressed/uncompressed according to threshold)
        let verify_token = [3u8, 2, 1, 0];
        let enc_req = CEncryptionRequest::new("", &server_public_key_der, &verify_token, false);
        let mut enc_req_buf = Vec::new();
        Client::write_packet(&enc_req, &mut enc_req_buf).unwrap();
        server_encoder
            .write_packet(enc_req_buf.into())
            .await
            .unwrap();
        server_encoder.flush().await.unwrap();

        // 4. Server receives SEncryptionResponse
        let raw_enc_resp = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_enc_resp.id, SEncryptionResponse::to_id(VERSION));
        let mut enc_resp_payload = &raw_enc_resp.payload[..];
        let enc_resp = SEncryptionResponse::read(&mut enc_resp_payload, &VERSION).unwrap();

        let decrypted_secret = server_private_key
            .decrypt(Pkcs1v15Encrypt, &enc_resp.shared_secret)
            .unwrap();
        let decrypted_token = server_private_key
            .decrypt(Pkcs1v15Encrypt, &enc_resp.verify_token)
            .unwrap();
        assert_eq!(decrypted_token.as_slice(), &verify_token);
        let shared_secret: [u8; 16] = decrypted_secret.try_into().unwrap();

        // Enable encryption on server (with compression already enabled)
        server_decoder.set_encryption(&shared_secret).unwrap();
        server_encoder.set_encryption(&shared_secret).unwrap();

        // 5. Server sends encrypted & compressed CLoginSuccess
        let bot_uuid = Uuid::new_v4();
        let login_success =
            CLoginSuccess::new(&bot_uuid, "BOT_COMPRESSED", &[], false, Uuid::new_v4());
        let mut ls_buf = Vec::new();
        Client::write_packet(&login_success, &mut ls_buf).unwrap();
        server_encoder.write_packet(ls_buf.into()).await.unwrap();
        server_encoder.flush().await.unwrap();

        // 6. Server receives encrypted & compressed SLoginAcknowledged & SKnownPacks
        let raw_login_ack = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_login_ack.id, SLoginAcknowledged::to_id(VERSION));

        let raw_known_packs = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_known_packs.id, SKnownPacks::to_id(VERSION));

        // 7. Server sends encrypted & compressed CFinishConfig
        let finish_config = CFinishConfig;
        let mut fc_buf = Vec::new();
        Client::write_packet(&finish_config, &mut fc_buf).unwrap();
        server_encoder.write_packet(fc_buf.into()).await.unwrap();
        server_encoder.flush().await.unwrap();

        // 8. Server receives encrypted & compressed SAcknowledgeFinishConfig
        let raw_ack_finish = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_ack_finish.id, SAcknowledgeFinishConfig::to_id(VERSION));

        // 9. Server sends encrypted & compressed CLogin and CPlayerPosition
        let spawn_data = PlayerSpawnData::new(
            Dimension::OVERWORLD.clone(),
            0,
            0,
            -1,
            false,
            false,
            None,
            VarInt(0),
            VarInt(63),
        );
        let login_packet = CLogin::new(
            200,
            false,
            &[],
            VarInt(20),
            VarInt(10),
            VarInt(10),
            false,
            true,
            false,
            spawn_data,
            false,
            false,
        );
        let mut login_buf = Vec::new();
        Client::write_packet(&login_packet, &mut login_buf).unwrap();
        server_encoder.write_packet(login_buf.into()).await.unwrap();

        let player_pos = CPlayerPosition::new(
            VarInt(77),
            Vector3::new(12.3, 65.4, 78.9),
            Vector3::new(0.0, 0.0, 0.0),
            180.0,
            10.0,
            Vec::new(),
        );
        let mut pos_buf = Vec::new();
        Client::write_packet(&player_pos, &mut pos_buf).unwrap();
        server_encoder.write_packet(pos_buf.into()).await.unwrap();
        server_encoder.flush().await.unwrap();

        // 10. Server receives encrypted & compressed SPlayerLoaded and SConfirmTeleport
        let raw_loaded = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_loaded.id, SPlayerLoaded::to_id(VERSION));

        let raw_confirm = server_decoder.get_raw_packet().await.unwrap();
        assert_eq!(raw_confirm.id, SConfirmTeleport::to_id(VERSION));
        let mut conf_payload = &raw_confirm.payload[..];
        let confirm = SConfirmTeleport::read(&mut conf_payload, &VERSION).unwrap();
        assert_eq!(confirm.teleport_id.0, 77);

        // Verify bot state
        assert_eq!(client.connection_state.load(), ConnectionState::Play);
        assert!(client.is_loaded.load(Ordering::Relaxed));
        assert_eq!(client.current_x.load(), 12.3);
        assert_eq!(client.current_y.load(), 65.4);
        assert_eq!(client.current_z.load(), 78.9);

        client.close().await;
        let _ = proc_handle.await;
    }
}

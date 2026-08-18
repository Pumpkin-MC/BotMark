use std::{net::SocketAddr, sync::Arc, sync::atomic::Ordering, time::Duration};

use clap::Parser;
use client::Client;
use tokio::{net::TcpStream, time::timeout};

mod client;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    #[arg(long)]
    pub ip: String,
    #[arg(short, long, default_value_t = 1)]
    pub count: u32,
    #[arg(short, long, default_value_t = 200)]
    pub delay: u64,
    #[arg(short, long, default_value_t = 5000)]
    pub timeout: u64,

    #[arg(long, default_value = "Please do not spam!")]
    pub spam_message: Option<String>,
    #[arg(long, default_value_t = 150)]
    pub spam_message_delay_min: u32,
    #[arg(long, default_value_t = 250)]
    pub spam_message_delay_max: u32,

    #[arg(long, default_value_t = true)]
    pub enable_rotation: bool,
    #[arg(long, default_value_t = true)]
    pub enable_swing: bool,
    #[arg(long, default_value_t = true)]
    pub enable_movement: bool,
    #[arg(long, default_value_t = true)]
    pub enable_jumping: bool,
    #[arg(long, default_value_t = true)]
    pub enable_physics: bool,
}

#[tokio::main]
async fn main() {
    simple_logger::init_with_level(log::Level::Info).unwrap();
    let args = Arc::new(Args::parse());
    let address = args.ip.parse::<SocketAddr>().unwrap();
    let bot_count = args.count;
    log::info!(
        "{} Bots will Join {} with a delay of {}ms",
        bot_count,
        address,
        args.delay
    );
    let timeout_dur = Duration::from_millis(args.timeout);
    let timeout_delay = Duration::from_millis(args.delay);

    let mut join_handles = Vec::with_capacity(bot_count as usize);
    for i in 0..bot_count {
        tokio::time::sleep(timeout_delay).await;
        let stream_result = timeout(timeout_dur, TcpStream::connect(address)).await;

        let stream = match stream_result {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                log::error!("Bot {} failed to connect to {}: {}", i + 1, address, e);
                continue;
            }
            Err(_) => {
                log::error!(
                    "Bot {} connection to {} timed out after {}ms",
                    i + 1,
                    address,
                    args.timeout
                );
                continue;
            }
        };
        let client = Arc::new(Client::new(stream));

        client.join_server(address, format!("BOT_{i}")).await;
        let cloned_args = args.clone();
        let client_tick = client.clone();
        let tick_handle = tokio::spawn(async move {
            let mut tick_interval = tokio::time::interval(Duration::from_millis(50));
            tick_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            while !client_tick.closed.load(Ordering::Relaxed) {
                tick_interval.tick().await;
                client_tick.tick(&cloned_args).await;
            }
        });

        let client_proc = client.clone();
        let proc_handle = tokio::spawn(async move {
            while client_proc.process_packets().await {
                // Keep reading and processing incoming packets without cancellation
            }
        });

        join_handles.push(tick_handle);
        join_handles.push(proc_handle);
        log::info!("{}/{} Bots Joined", i + 1, bot_count);
    }
    // Graceful shutdown on Ctrl+C
    tokio::signal::ctrl_c().await.unwrap();
    log::info!("Shutting down...");

    // Wait for all bot tasks to exit
    for handle in join_handles {
        handle.abort();
    }
}

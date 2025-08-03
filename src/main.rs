use std::{net::SocketAddr, sync::Arc, thread::sleep, time::Duration};

use clap::Parser;
use client::Client;
use tokio::net::TcpStream;

mod client;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    #[arg(long)]
    ip: String,
    #[arg(short, long, default_value_t = 1)]
    count: u32,
    #[arg(short, long, default_value_t = 200)]
    delay: u64,
    #[arg(long)]
    pub spam_message: Option<String>,
    #[arg(long, default_value_t = 210)]
    pub spam_message_delay: u32,
}

#[tokio::main]
async fn main() {
    simple_logger::init_with_level(log::Level::Info).unwrap();
    let args = Args::parse();
    let address = args.ip.parse::<SocketAddr>().unwrap();
    log::info!(
        "{} Bots will Join {} with a delay of {}ms",
        args.count,
        address,
        args.delay
    );
    let mut join_handles = Vec::with_capacity(args.count as usize);
    for i in 0..args.count {
        sleep(Duration::from_millis(args.delay));
        let stream = TcpStream::connect(address)
            .await
            .expect("Failed to connect to Ip");
        let client = Arc::new(Client::new(stream));
        let message = Arc::new(args.spam_message.clone());

        client.join_server(address, format!("BOT_{i}")).await;
        let join_handle = tokio::spawn(async move {
            loop {
                if !client.process_packets().await {
                    break;
                }
                client.tick(message.clone(), args.spam_message_delay).await;
            }
        });
        join_handles.push(join_handle);
        log::info!("{}/{} Bots Joined", i + 1, args.count);
    }
    // Graceful shutdown on Ctrl+C
    tokio::signal::ctrl_c().await.unwrap();
    log::info!("Shutting down...");

    // Wait for all bot tasks to exit
    for handle in join_handles {
        handle.abort();
    }
}

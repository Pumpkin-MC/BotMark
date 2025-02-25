use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};

use clap::Parser;
use client::Client;
use tokio::net::TcpStream;

mod client;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    ip: String,
    #[arg(short, long, default_value_t = 25565)]
    port: u16,
    #[arg(short, long, default_value_t = 1)]
    count: u32,
}

#[tokio::main]
async fn main() {
    simple_logger::init_with_level(log::Level::Info).unwrap();
    let args = Args::parse();
    let address = Arc::new(args.ip);
    let port = args.port;

    log::info!("{} Bots will Join {}", args.count, address);

    let counter = Arc::new(AtomicUsize::new(0));
    for _ in 0..args.count {
        let counter = counter.clone();
        let address = address.clone();

        tokio::spawn(async move {
            let stream = TcpStream::connect(address.to_string() + ":" + &port.to_string())
                .await
                .expect("Failed to connect to Ip");
            let client = Client::new(stream);
            let i = counter.fetch_add(1, Ordering::Relaxed);
            
            client.join_server(address.to_string(), port, format!("BOT_{i}")).await;
            log::info!("{}/{} Bots Joined", i + 1, args.count);
    
            loop {
                if !client.poll().await {
                    break;
                }
                client.process_packets().await;
            }
        });
    };

    // Graceful shutdown on Ctrl+C
    tokio::signal::ctrl_c().await.unwrap();
    log::info!("Shutting down...");
}


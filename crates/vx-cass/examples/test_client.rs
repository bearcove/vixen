//! Simple test client to verify vx-cass TCP connectivity

use eyre::Result;
use std::sync::Arc;
use tokio::net::TcpStream;
use vx_cass_proto::CassClient;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("test_client=debug")
        .init();

    let addr = "127.0.0.1:9002";
    tracing::info!("Connecting to CAS at {}...", addr);

    // Connect to server
    let stream = TcpStream::connect(addr).await?;
    tracing::info!("Connected!");

    // Create rapace transport and session
    let session = Arc::new(rapace::RpcSession::new(rapace::Transport::stream(stream)));

    // Spawn session runner to handle incoming frames
    let session_runner = session.clone();
    tokio::spawn(async move {
        if let Err(e) = session_runner.run().await {
            tracing::error!("Session error: {}", e);
        }
    });

    // Create CAS client
    let client = CassClient::new(session.clone());

    // Test: put a blob
    tracing::info!("Testing put_blob...");
    let test_data = b"Hello from vx-cass test client!";
    let blob_hash = client.put_blob(test_data.to_vec()).await?;
    tracing::info!("Put blob: {}", blob_hash.to_hex());

    // Test: get the blob back
    tracing::info!("Testing get_blob...");
    let retrieved = client.get_blob(blob_hash).await?;
    match retrieved {
        Some(data) => {
            assert_eq!(data, test_data);
            tracing::info!("Retrieved blob: {}", String::from_utf8_lossy(&data));
        }
        None => {
            tracing::error!("Failed to retrieve blob!");
            return Err(eyre::eyre!("Blob not found"));
        }
    }

    // Test: has_blob
    tracing::info!("Testing has_blob...");
    let exists = client.has_blob(blob_hash).await?;
    tracing::info!("Blob exists: {}", exists);

    // Close connection
    session.close();
    tracing::info!("Done!");

    Ok(())
}

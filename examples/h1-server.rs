use std::net::TcpListener;

use anyhow::Result;
use futures::prelude::*;
use http_types::{Request, Response, StatusCode};
use async_dup::Arc;
use bastion::executor::spawn;
use proactor::*;

/// Serves a request and returns a response.
async fn serve(req: Request) -> http_types::Result<Response> {
    println!("Serving {}", req.url());

    let mut res = Response::new(StatusCode::Ok);
    res.insert_header("Content-Type", "text/plain")?;
    res.set_body("Hello from async-h1!");
    Ok(res)
}

/// Listens for incoming connections and serves them.
async fn listen(listener: Handle<TcpListener>) -> Result<()> {
    // Format the full host address.
    let host = format!("http://{}", listener.get_ref().local_addr()?);
    println!("Listening on {}", host);

    loop {
        // Accept the next connection.
        let (stream, _) = listener.accept().await?;
        let host = host.clone();

        // Spawn a background task serving this connection.
        let stream = Arc::new(stream);
        spawn(async move {
            if let Err(err) = async_h1::accept(&host, stream, serve).await {
                println!("Connection error: {:#?}", err);
            }
        });
    }
}

fn main() -> Result<()> {
    // Start HTTP and HTTPS servers.
    run(async {
        let http = listen(Handle::<TcpListener>::bind("127.0.0.1:8000")?);
        http.await?;
        Ok(())
    })
}

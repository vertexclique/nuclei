use nuclei::*;
use std::net::TcpListener;

use anyhow::Result;
use async_dup::Arc;

use futures::prelude::*;
use http_types::{Request, Response, StatusCode};

static DATA: &'static str = include_str!("data/quark-gluon-plasma");

/// Serves a request and returns a response.
async fn serve(req: Request) -> http_types::Result<Response> {
    // println!("Serving {}", req.url());

    let mut res = Response::new(StatusCode::Ok);
    res.insert_header("Content-Type", "text/plain");
    res.set_body(DATA);
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

        // Spawn a background task serving this connection.
        let stream = Arc::new(stream);
        spawn(async move {
            if let Err(err) = async_h1::accept(stream, serve).await {
                println!("Connection error: {:#?}", err);
            }
        })
        .detach();
    }
}

#[nuclei::main]
async fn main() -> Result<()> {
    let http = listen(Handle::<TcpListener>::bind("0.0.0.0:8000")?);

    http.await?;
    Ok(())
}

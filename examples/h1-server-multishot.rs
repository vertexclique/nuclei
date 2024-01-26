#[cfg(target_os = "linux")]
#[nuclei::main]
async fn main() -> anyhow::Result<()> {
    use nuclei::*;
    use std::net::TcpListener;

    use anyhow::Result;
    use async_dup::Arc;
    use futures::stream::StreamExt;

    use futures::prelude::*;
    use http_types::{Request, Response, StatusCode};

    /////////////////////////////////////////////////////////////////////////
    ////// NOTE: This example can only be run by IO_URING backend.
    ////// If you try to use epoll, it will not compile.
    ////// Reason is: Multishot based IO is only part of io_uring backend.
    /////////////////////////////////////////////////////////////////////////

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

        let mut streams = listener.accept_multi().await?;

        while let Some(stream) = streams.next().await {
            // Spawn a background task serving this connection.
            let stream = Arc::new(stream);
            spawn(async move {
                if let Err(err) = async_h1::accept(stream, serve).await {
                    println!("Connection error: {:#?}", err);
                }
            })
            .detach();
        }

        Ok(())
    }

    let http = listen(Handle::<TcpListener>::bind("0.0.0.0:8000")?);

    http.await?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn main() {
    panic!("This example can only be run by IO_URING backend.");
}

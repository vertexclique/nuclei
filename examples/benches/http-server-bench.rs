use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput, BenchmarkId};
use nuclei::block_on;
use criterion::async_executor::FuturesExecutor;


use nuclei::*;
use std::net::TcpListener;

use anyhow::Result;
use async_dup::Arc;

use futures::prelude::*;
use futures_util::future::join_all;
use http_types::{Request, Response, StatusCode};

static DATA: &'static str = include_str!("../data/quark-gluon-plasma");

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

fn server() -> Result<()> {
    spawn_blocking(|| drive(future::pending::<()>())).detach();

    block_on(async {
        let http = listen(Handle::<TcpListener>::bind("0.0.0.0:8000")?);

        http.await?;
        Ok(())
    })
}

pub fn http_server_bench(c: &mut Criterion) {
    let x = nuclei::spawn_blocking(|| server());

    let uri = "http://127.0.0.1:8000";

    let mut group = c.benchmark_group("http_server_bench");
    for i in [1_u64, 10_u64, 25_u64].iter() {
        group.throughput(Throughput::Bytes(DATA.len() as u64 * i));
        group.bench_function(BenchmarkId::from_parameter(i), |b| b.to_async(FuturesExecutor).iter(|| async {
            let tasks = (0..*i).map(|e| surf::get(uri).recv_string()).collect::<Vec<_>>();
            join_all(tasks).await;
        }));
    }
    group.finish();

    nuclei::block_on(x.cancel());
}

criterion_group!(benches, http_server_bench);
criterion_main!(benches);
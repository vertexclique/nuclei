use nuclei::RuntimeHandle;
use nuclei::sync::mesh::mesh;

#[nuclei::main(threads = 3)]
async fn main() -> std::io::Result<()> {
    let runtime = RuntimeHandle::current();
    // One party per worker, each pair joined by a channel of 8 messages each way.
    let parties = mesh::<usize>(runtime.workers(), 8);
    let mut tasks = Vec::new();
    for (worker, mut party) in parties.into_iter().enumerate() {
        tasks.push(runtime.spawn_on_worker(worker, async move {
            // Every party sends its index to every party, itself included.
            for to in 0..party.parties() {
                party.send_to(to, party.index()).await.unwrap();
            }
            // recv ends once every party, this one too, has closed its senders.
            party.close_senders();
            let mut heard = Vec::new();
            while let Some((from, index)) = party.recv().await {
                assert_eq!(from, index);
                heard.push(from);
            }
            heard.sort_unstable();
            heard
        })?);
    }
    for task in tasks {
        assert_eq!(task.await, [0, 1, 2]);
    }
    Ok(())
}

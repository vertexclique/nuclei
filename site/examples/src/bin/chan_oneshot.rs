use nuclei::sync::oneshot;

#[nuclei::main]
async fn main() {
    // One value, once: a reply to a request.
    let (reply, answer) = oneshot::channel::<u64>();
    nuclei::spawn(async move {
        let _ = reply.send(6 * 7);
    })
    .detach();
    assert_eq!(answer.await.unwrap(), 42);

    // A sender dropped before it sent: the receiver gets an error.
    let (reply, answer) = oneshot::channel::<u64>();
    drop(reply);
    assert!(answer.await.is_err());
}

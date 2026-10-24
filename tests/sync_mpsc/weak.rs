//! Weak senders and the sender counts: upgrading while a strong sender
//! exists, the end of the channel with only weak senders left, the counts the
//! senders, weak senders and the receiver report.

use super::*;
use nuclei::sync::oneshot;

#[test]
fn a_weak_sender_upgrades_to_send_while_a_strong_one_exists() {
    let (tx, mut rx) = mpsc::channel(11);

    let tx_weak = std::thread::spawn(move || {
        let tx_weak = tx.clone().downgrade();

        for i in 0..10 {
            if block_on(tx.send(i)).is_err() {
                return None;
            }
        }

        let tx2 = tx_weak
            .upgrade()
            .expect("expected to be able to upgrade tx_weak");
        let _ = block_on(tx2.send(20));
        let tx_weak = tx2.downgrade();

        Some(tx_weak)
    })
    .join()
    .unwrap();

    for i in 0..12 {
        match block_on(rx.recv()) {
            Some(msg) => {
                if i == 10 {
                    assert_eq!(msg, 20);
                }
            }
            None => {
                assert_eq!(i, 11);
                break;
            }
        }
    }

    let tx_weak = tx_weak.unwrap();
    assert!(
        tx_weak.upgrade().is_none(),
        "every strong sender is gone, so the weak one cannot upgrade"
    );
}

#[test]
fn a_weak_unbounded_sender_upgrades_to_send_while_a_strong_one_exists() {
    let (tx, mut rx) = mpsc::unbounded_channel();

    let tx_weak = std::thread::spawn(move || {
        let tx_weak = tx.clone().downgrade();

        for i in 0..10 {
            if tx.send(i).is_err() {
                return None;
            }
        }

        let tx2 = tx_weak
            .upgrade()
            .expect("expected to be able to upgrade tx_weak");
        let _ = tx2.send(20);
        Some(tx2.downgrade())
    })
    .join()
    .unwrap();

    for i in 0..12 {
        match block_on(rx.recv()) {
            Some(msg) => {
                if i == 10 {
                    assert_eq!(msg, 20);
                }
            }
            None => {
                assert_eq!(i, 11);
                break;
            }
        }
    }

    assert!(tx_weak.unwrap().upgrade().is_none());
}

#[test]
fn a_bounded_actor_keeps_a_weak_handle_to_its_own_mailbox() {
    pub struct MyActor {
        receiver: mpsc::Receiver<ActorMessage>,
        sender: mpsc::WeakSender<ActorMessage>,
        next_id: u32,
        pub received_self_msg: bool,
    }

    enum ActorMessage {
        GetUniqueId { respond_to: oneshot::Sender<u32> },
        SelfMessage {},
    }

    impl MyActor {
        fn handle_message(&mut self, msg: ActorMessage) {
            match msg {
                ActorMessage::GetUniqueId { respond_to } => {
                    self.next_id += 1;
                    let _ = respond_to.send(self.next_id);
                }
                ActorMessage::SelfMessage { .. } => {
                    self.received_self_msg = true;
                }
            }
        }

        async fn send_message_to_self(&mut self) {
            let msg = ActorMessage::SelfMessage {};

            let sender = self.sender.clone();

            // The sender cannot be moved out of `self`, so it is cloned.
            if let Some(sender) = sender.upgrade() {
                let _ = sender.send(msg).await;
                self.sender = sender.downgrade();
            }
        }

        async fn run(&mut self) {
            let mut i = 0;
            while let Some(msg) = self.receiver.recv().await {
                self.handle_message(msg);

                if i == 0 {
                    self.send_message_to_self().await;
                }

                i += 1
            }

            assert!(self.received_self_msg);
        }
    }

    let (sender, receiver) = mpsc::channel(8);
    let mut actor = MyActor {
        receiver,
        sender: sender.clone().downgrade(),
        next_id: 0,
        received_self_msg: false,
    };

    let actor_thread = std::thread::spawn(move || block_on(actor.run()));

    // The actor sends itself a message while it handles the first request, so
    // the handle must stay until the actor has gone on to the second one.
    for expected in 1..=2 {
        let (send, recv) = oneshot::channel();
        block_on(sender.send(ActorMessage::GetUniqueId { respond_to: send })).unwrap();
        assert_eq!(block_on(recv).expect("the actor answered"), expected);
    }
    drop(sender);

    actor_thread.join().unwrap();
}

#[test]
fn an_unbounded_actor_keeps_a_weak_handle_to_its_own_mailbox() {
    pub struct MyActor {
        receiver: mpsc::UnboundedReceiver<ActorMessage>,
        sender: mpsc::WeakUnboundedSender<ActorMessage>,
        next_id: u32,
        pub received_self_msg: bool,
    }

    enum ActorMessage {
        GetUniqueId { respond_to: oneshot::Sender<u32> },
        SelfMessage {},
    }

    impl MyActor {
        fn handle_message(&mut self, msg: ActorMessage) {
            match msg {
                ActorMessage::GetUniqueId { respond_to } => {
                    self.next_id += 1;
                    let _ = respond_to.send(self.next_id);
                }
                ActorMessage::SelfMessage { .. } => {
                    self.received_self_msg = true;
                }
            }
        }

        async fn send_message_to_self(&mut self) {
            let msg = ActorMessage::SelfMessage {};

            let sender = self.sender.clone();

            if let Some(sender) = sender.upgrade() {
                let _ = sender.send(msg);
                self.sender = sender.downgrade();
            }
        }

        async fn run(&mut self) {
            let mut i = 0;
            while let Some(msg) = self.receiver.recv().await {
                self.handle_message(msg);

                if i == 0 {
                    self.send_message_to_self().await;
                }

                i += 1
            }

            assert!(self.received_self_msg);
        }
    }

    let (sender, receiver) = mpsc::unbounded_channel();
    let mut actor = MyActor {
        receiver,
        sender: sender.clone().downgrade(),
        next_id: 0,
        received_self_msg: false,
    };

    let actor_thread = std::thread::spawn(move || block_on(actor.run()));

    // See the bounded actor: the handle stays until the second request is answered.
    for expected in 1..=2 {
        let (send, recv) = oneshot::channel();
        sender
            .send(ActorMessage::GetUniqueId { respond_to: send })
            .unwrap();
        assert_eq!(block_on(recv).expect("the actor answered"), expected);
    }
    drop(sender);

    actor_thread.join().unwrap();
}

#[test]
fn downgrade_then_upgrade_works_while_another_sender_exists() {
    let (tx, _rx) = mpsc::channel::<i32>(1);
    let weak_tx = tx.downgrade();
    assert!(weak_tx.upgrade().is_some());

    let (tx, _rx) = mpsc::unbounded_channel::<i32>();
    let weak_tx = tx.downgrade();
    assert!(weak_tx.upgrade().is_some());
}

#[test]
fn a_weak_sender_cannot_upgrade_after_the_last_strong_sender_is_dropped() {
    let (tx, _rx) = mpsc::channel::<i32>(1);
    let weak_tx = tx.downgrade();
    drop(tx);
    assert!(weak_tx.upgrade().is_none());

    let (tx, _rx) = mpsc::unbounded_channel::<i32>();
    let weak_tx = tx.downgrade();
    drop(tx);
    assert!(weak_tx.upgrade().is_none());
}

#[test]
fn a_clone_dropped_right_away_does_not_keep_a_weak_sender_alive() {
    let (tx, _rx) = mpsc::channel::<i32>(1);
    let weak_tx = tx.clone().downgrade();
    drop(tx);
    assert!(weak_tx.upgrade().is_none());

    let (tx, _rx) = mpsc::unbounded_channel::<i32>();
    let weak_tx = tx.clone().downgrade();
    drop(tx);
    assert!(weak_tx.upgrade().is_none());
}

#[test]
fn a_weak_sender_upgrades_through_an_outstanding_owned_permit() {
    // The permit holds the only strong sender.
    let (tx, _rx) = mpsc::channel::<i32>(1);
    let weak_tx = tx.downgrade();
    let _permit = block_on(tx.reserve_owned()).unwrap();
    assert!(weak_tx.upgrade().is_some());

    // The permit holds the last strong sender after another is dropped.
    let (tx, _rx) = mpsc::channel::<i32>(1);
    let tx2 = tx.clone();
    let _permit = block_on(tx.reserve_owned()).unwrap();
    let weak_tx = tx2.downgrade();
    drop(tx2);
    assert!(weak_tx.upgrade().is_some());
}

#[test]
fn downgrading_does_not_change_the_strong_count() {
    let (tx, _rx) = mpsc::channel::<i32>(1);
    let tx_weak = tx.downgrade();
    let tx_weak2 = tx.downgrade();
    drop(tx);

    assert!(tx_weak.upgrade().is_none() && tx_weak2.upgrade().is_none());

    let (tx, _rx) = mpsc::unbounded_channel::<i32>();
    let tx_weak = tx.downgrade();
    let tx_weak2 = tx.downgrade();
    drop(tx);

    assert!(tx_weak.upgrade().is_none() && tx_weak2.upgrade().is_none());
}

#[test]
fn the_channel_is_closed_when_only_weak_senders_remain() {
    let (tx, rx) = mpsc::channel::<()>(10);
    let _weak_sender = tx.clone().downgrade();
    drop(tx);
    assert!(rx.is_closed());

    let (tx, rx) = mpsc::unbounded_channel::<()>();
    let _weak_sender = tx.clone().downgrade();
    drop(tx);
    assert!(rx.is_closed());
}

#[test]
fn a_weak_sender_alone_ends_recv_with_none() {
    let (tx, mut rx) = mpsc::channel::<u8>(2);
    let weak = tx.downgrade();
    tx.try_send(1).unwrap();
    drop(tx);
    assert_eq!(block_on(rx.recv()), Some(1));
    assert_eq!(block_on(rx.recv()), None);
    assert!(weak.upgrade().is_none());
}

#[test]
fn a_weak_sender_clone_and_drop_keep_the_weak_count() {
    let (tx, rx) = mpsc::channel::<()>(1);
    let weak = tx.downgrade();
    let weak2 = weak.clone();
    assert_eq!(tx.weak_count(), 2);
    drop(weak);
    assert_eq!(weak2.weak_count(), 1);
    assert_eq!(rx.sender_weak_count(), 1);
    drop(weak2);
    assert_eq!(rx.sender_weak_count(), 0);
}

#[test]
fn sender_counts_follow_clones_and_drops_of_the_bounded_sender() {
    let (tx, rx) = mpsc::channel::<()>(1);

    let tx2 = tx.clone();

    assert_eq!(tx.strong_count(), 2);
    assert_eq!(tx2.strong_count(), 2);
    assert_eq!(rx.sender_strong_count(), 2);

    drop(tx2);

    assert_eq!(tx.strong_count(), 1);
    assert_eq!(rx.sender_strong_count(), 1);
}

#[test]
fn weak_counts_follow_downgrades_and_drops_of_the_bounded_sender() {
    let (tx, rx) = mpsc::channel::<()>(1);

    let weak = tx.downgrade();

    assert_eq!(tx.weak_count(), 1);
    assert_eq!(weak.weak_count(), 1);
    assert_eq!(rx.sender_weak_count(), 1);

    drop(weak);

    assert_eq!(tx.weak_count(), 0);
    assert_eq!(rx.sender_weak_count(), 0);
}

#[test]
fn strong_and_weak_counts_together_for_the_bounded_sender() {
    let (tx, rx) = mpsc::channel::<()>(1);

    let tx2 = tx.clone();

    let weak = tx.downgrade();
    let weak2 = tx2.downgrade();

    assert_eq!(tx.strong_count(), 2);
    assert_eq!(tx2.strong_count(), 2);
    assert_eq!(weak.strong_count(), 2);
    assert_eq!(weak2.strong_count(), 2);
    assert_eq!(rx.sender_strong_count(), 2);

    assert_eq!(tx.weak_count(), 2);
    assert_eq!(tx2.weak_count(), 2);
    assert_eq!(weak.weak_count(), 2);
    assert_eq!(weak2.weak_count(), 2);
    assert_eq!(rx.sender_weak_count(), 2);

    drop(tx2);
    drop(weak2);

    assert_eq!(tx.strong_count(), 1);
    assert_eq!(weak.strong_count(), 1);
    assert_eq!(rx.sender_strong_count(), 1);

    assert_eq!(tx.weak_count(), 1);
    assert_eq!(weak.weak_count(), 1);
    assert_eq!(rx.sender_weak_count(), 1);
}

#[test]
fn sender_counts_follow_clones_and_drops_of_the_unbounded_sender() {
    let (tx, rx) = mpsc::unbounded_channel::<()>();

    let tx2 = tx.clone();

    assert_eq!(tx.strong_count(), 2);
    assert_eq!(tx2.strong_count(), 2);
    assert_eq!(rx.sender_strong_count(), 2);

    drop(tx2);

    assert_eq!(tx.strong_count(), 1);
    assert_eq!(rx.sender_strong_count(), 1);
}

#[test]
fn weak_counts_follow_downgrades_and_drops_of_the_unbounded_sender() {
    let (tx, rx) = mpsc::unbounded_channel::<()>();

    let weak = tx.downgrade();

    assert_eq!(tx.weak_count(), 1);
    assert_eq!(weak.weak_count(), 1);
    assert_eq!(rx.sender_weak_count(), 1);

    drop(weak);

    assert_eq!(tx.weak_count(), 0);
    assert_eq!(rx.sender_weak_count(), 0);
}

#[test]
fn strong_and_weak_counts_together_for_the_unbounded_sender() {
    let (tx, rx) = mpsc::unbounded_channel::<()>();

    let tx2 = tx.clone();

    let weak = tx.downgrade();
    let weak2 = tx2.downgrade();

    assert_eq!(tx.strong_count(), 2);
    assert_eq!(tx2.strong_count(), 2);
    assert_eq!(weak.strong_count(), 2);
    assert_eq!(weak2.strong_count(), 2);
    assert_eq!(rx.sender_strong_count(), 2);

    assert_eq!(tx.weak_count(), 2);
    assert_eq!(tx2.weak_count(), 2);
    assert_eq!(weak.weak_count(), 2);
    assert_eq!(weak2.weak_count(), 2);
    assert_eq!(rx.sender_weak_count(), 2);

    drop(tx2);
    drop(weak2);

    assert_eq!(tx.strong_count(), 1);
    assert_eq!(weak.strong_count(), 1);
    assert_eq!(rx.sender_strong_count(), 1);

    assert_eq!(tx.weak_count(), 1);
    assert_eq!(weak.weak_count(), 1);
    assert_eq!(rx.sender_weak_count(), 1);
}

#[test]
fn threads_upgrading_and_dropping_leave_the_count_exact() {
    let rounds = scale(2_000, 20) as usize;
    let (tx, rx) = mpsc::channel::<u8>(1);
    let weak = tx.downgrade();
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let weak = weak.clone();
            std::thread::spawn(move || {
                for _ in 0..rounds {
                    if let Some(sender) = weak.upgrade() {
                        drop(sender.clone());
                    }
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(rx.sender_strong_count(), 1);
    assert_eq!(rx.sender_weak_count(), 1);
    drop(tx);
    assert_eq!(rx.sender_strong_count(), 0);
    assert!(weak.upgrade().is_none());
}

use std::cell::Cell;
use std::future::Future;
use std::rc::Rc;

use nuclei::runtime::Runtime;
use nuclei::sync::local::{Mutex, Semaphore, mpsc, oneshot};
use nuclei::task::{self, coop};

fn runtime() -> Runtime {
    Runtime::builder()
        .worker_threads(1)
        .pin_workers(false)
        .build()
        .expect("the runtime starts")
}

/// Runs a busy task and, once it has started, a task that only raises a
/// flag; answers the iteration at which the busy task first saw the flag
/// (`None`: it never yielded before its end).
fn first_sight<F, Fut>(unconstrained: bool, busy: F) -> Option<usize>
where
    F: FnOnce(Rc<Cell<bool>>, Rc<Cell<bool>>) -> Fut,
    Fut: Future<Output = Option<usize>> + 'static,
{
    runtime().block_on(async move {
        let flag = Rc::new(Cell::new(false));
        let started = Rc::new(Cell::new(false));
        let work = busy(Rc::clone(&flag), Rc::clone(&started));
        let busy = if unconstrained {
            task::spawn_local(async move { coop::unconstrained(work).await })
        } else {
            task::spawn_local(work)
        };
        while !started.get() {
            task::yield_now().await;
        }
        let raiser = task::spawn_local(async move { flag.set(true) });
        let first = busy.await;
        raiser.await;
        first
    })
}

const LIMIT: usize = 3 * coop::BUDGET as usize;

async fn unbounded_loop(flag: Rc<Cell<bool>>, started: Rc<Cell<bool>>) -> Option<usize> {
    started.set(true);
    let (tx, mut rx) = mpsc::unbounded_channel();
    for i in 0..1000 {
        if flag.get() {
            return Some(i);
        }
        tx.send(i).unwrap();
        rx.recv().await;
    }
    None
}

async fn bounded_loop(flag: Rc<Cell<bool>>, started: Rc<Cell<bool>>) -> Option<usize> {
    started.set(true);
    let (tx, mut rx) = mpsc::channel(4);
    for i in 0..1000 {
        if flag.get() {
            return Some(i);
        }
        tx.send(i).await.unwrap();
        rx.recv().await;
    }
    None
}

async fn mutex_loop(flag: Rc<Cell<bool>>, started: Rc<Cell<bool>>) -> Option<usize> {
    started.set(true);
    let mutex = Mutex::new(0);
    for i in 0..1000 {
        if flag.get() {
            return Some(i);
        }
        *mutex.lock().await += 1;
    }
    None
}

async fn semaphore_loop(flag: Rc<Cell<bool>>, started: Rc<Cell<bool>>) -> Option<usize> {
    started.set(true);
    let sem = Semaphore::new(1);
    for i in 0..1000 {
        if flag.get() {
            return Some(i);
        }
        drop(sem.acquire().await.unwrap());
    }
    None
}

async fn oneshot_loop(flag: Rc<Cell<bool>>, started: Rc<Cell<bool>>) -> Option<usize> {
    started.set(true);
    for i in 0..1000 {
        if flag.get() {
            return Some(i);
        }
        let (tx, rx) = oneshot::channel();
        tx.send(i).unwrap();
        rx.await.unwrap();
    }
    None
}

macro_rules! budget_tests {
    ($($name:ident: $busy:ident),* $(,)?) => {$(
        mod $name {
            use super::*;

            #[test]
            fn a_loop_that_is_always_ready_lets_another_task_run() {
                let first = first_sight(false, $busy).expect("the other task never ran");
                assert!(first <= LIMIT, "ran {first} iterations before the other task got a turn");
            }

            #[test]
            fn an_unconstrained_loop_never_yields() {
                assert_eq!(first_sight(true, $busy), None);
            }
        }
    )*};
}

budget_tests! {
    unbounded_channel: unbounded_loop,
    bounded_channel: bounded_loop,
    mutex: mutex_loop,
    semaphore: semaphore_loop,
    oneshot_channel: oneshot_loop,
}

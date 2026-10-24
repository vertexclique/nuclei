use super::{CLASSES, Pool, enter};

#[test]
fn a_request_takes_the_smallest_class_that_holds_it() {
    let pool = Pool::new(1 << 20);
    assert_eq!(pool.get(1).unwrap().capacity(), 512);
    assert_eq!(pool.get(512).unwrap().capacity(), 512);
    assert_eq!(pool.get(513).unwrap().capacity(), 4096);
    assert_eq!(pool.get(20_000).unwrap().capacity(), 64 * 1024);
    assert_eq!(
        pool.get(1 << 20).unwrap().capacity(),
        64 * 1024,
        "the largest class"
    );
}

#[test]
fn a_buffer_dropped_on_its_shard_is_reused() {
    let pool = Pool::new(1 << 20);
    let _entered = enter(&pool);
    let a = pool.get(4096).unwrap();
    let addr = a.filled().as_ptr() as usize;
    drop(a);
    let b = pool.get(4096).unwrap();
    assert_eq!(b.filled().as_ptr() as usize, addr, "the same memory");
    assert_eq!(pool.charged(), 4096, "nothing more was allocated");
}

#[test]
fn the_pool_takes_nothing_past_its_cap() {
    let pool = Pool::new(8 * 1024);
    let _entered = enter(&pool);
    let a = pool.get(4096).unwrap();
    let b = pool.get(4096).unwrap();
    assert!(pool.get(4096).is_none(), "at the cap");
    assert!(pool.get(64 * 1024).is_none());
    assert_eq!(pool.charged(), 8 * 1024);
    drop(a);
    assert!(
        pool.get(4096).is_some(),
        "a freed buffer serves the next request"
    );
    drop(b);
}

#[test]
fn a_buffer_dropped_on_another_thread_comes_back_through_the_queue() {
    let pool = Pool::new(1 << 20);
    let _entered = enter(&pool);
    let a = pool.get(512).unwrap();
    let addr = a.filled().as_ptr() as usize;
    std::thread::spawn(move || drop(a)).join().unwrap();
    let b = pool.get(512).unwrap();
    assert_eq!(b.filled().as_ptr() as usize, addr);
    assert_eq!(pool.charged(), 512);
}

#[test]
fn buffers_outliving_their_pool_are_freed_by_its_home() {
    let pool = Pool::new(1 << 20);
    let bufs: Vec<_> = CLASSES.iter().map(|&c| pool.get(c).unwrap()).collect();
    drop(pool);
    drop(bufs);
}

#[test]
fn filled_bytes_are_read_out_in_order() {
    let pool = Pool::new(1 << 20);
    let _entered = enter(&pool);
    let mut b = pool.get(16).unwrap();
    assert!(b.is_empty());
    assert_eq!(b.fill_from(b"hello world"), 11);
    let mut out = [0u8; 5];
    assert_eq!(b.read_into(&mut out), 5);
    assert_eq!(&out, b"hello");
    assert_eq!(b.filled(), b" world");
    b.consume(1);
    let mut rest = [0u8; 16];
    assert_eq!(b.read_into(&mut rest), 5);
    assert_eq!(&rest[..5], b"world");
    assert!(b.is_empty());
    // A fill starts over from the buffer's start.
    let spare = b.spare();
    spare[..3].copy_from_slice(b"abc");
    b.set_filled(3);
    assert_eq!(b.filled(), b"abc");
}

#[test]
fn a_fill_larger_than_the_buffer_takes_what_fits() {
    let pool = Pool::new(1 << 20);
    let mut b = pool.get(1).unwrap();
    let big = vec![7u8; 1000];
    assert_eq!(b.fill_from(&big), 512);
    assert_eq!(b.filled().len(), 512);
}

#[test]
fn a_whole_buffer_holds_a_request_past_the_largest_class_and_is_not_kept() {
    let pool = Pool::new(1 << 20);
    let _entered = enter(&pool);
    let b = pool.get_whole(100_000).unwrap();
    assert!(b.capacity() >= 100_000);
    assert_eq!(b.capacity() % 4096, 0);
    assert_eq!(pool.charged(), b.capacity());
    drop(b);
    assert_eq!(pool.charged(), 0, "freed when it came back, not kept");
    // A whole request that a class holds takes the class.
    assert_eq!(pool.get_whole(3000).unwrap().capacity(), 4096);
}

#[test]
fn a_whole_buffer_dropped_elsewhere_is_freed_when_the_pool_looks() {
    let pool = Pool::new(1 << 20);
    let _entered = enter(&pool);
    let b = pool.get_whole(200_000).unwrap();
    let size = b.capacity();
    std::thread::spawn(move || drop(b)).join().unwrap();
    assert_eq!(pool.charged(), size, "still charged until the pool looks");
    let again = pool.get_whole(200_000).unwrap();
    assert_eq!(pool.charged(), size, "the returned one was freed first");
    drop(again);
}

#[test]
fn at_the_cap_idle_buffers_of_other_classes_make_room() {
    let pool = Pool::new(64 * 1024);
    let _entered = enter(&pool);
    let small: Vec<_> = (0..128).map(|_| pool.get(512).unwrap()).collect();
    assert_eq!(pool.charged(), 64 * 1024);
    assert!(pool.get(64 * 1024).is_none(), "all in use");
    drop(small);
    let big = pool.get(64 * 1024).unwrap();
    assert_eq!(big.capacity(), 64 * 1024);
    assert_eq!(pool.charged(), 64 * 1024, "the idle ones were freed for it");
}

#[test]
fn a_request_takes_the_largest_class_the_cap_allows_but_a_whole_one_does_not_shrink() {
    let pool = Pool::new(16 * 1024);
    assert_eq!(pool.get(64 * 1024).unwrap().capacity(), 16 * 1024);
    assert!(
        pool.get_whole(20_000).is_none(),
        "a whole datagram is never cut"
    );
    assert!(Pool::new(100).get(1).is_none(), "no class fits");
}

#[test]
fn appended_bytes_follow_the_unread_ones_moving_them_to_the_front() {
    let pool = Pool::new(1 << 20);
    let mut b = pool.get(512).unwrap();
    assert_eq!(b.fill_from(&[1u8; 500]), 500);
    b.consume(400);
    // 12 bytes of room at the end, 400 at the front: the unread 100 move.
    assert_eq!(b.append(&[2u8; 300]), 300);
    assert_eq!(b.filled().len(), 400);
    assert!(b.filled()[..100].iter().all(|&x| x == 1));
    assert!(b.filled()[100..].iter().all(|&x| x == 2));
    // Full: what does not fit is left.
    assert_eq!(b.append(&[3u8; 200]), 112);
    assert_eq!(b.filled().len(), 512);
}

struct Woken(std::sync::atomic::AtomicUsize);

impl std::task::Wake for Woken {
    fn wake(self: std::sync::Arc<Self>) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn waker() -> (std::sync::Arc<Woken>, std::task::Waker) {
    let w = std::sync::Arc::new(Woken(std::sync::atomic::AtomicUsize::new(0)));
    (w.clone(), std::task::Waker::from(w))
}

#[test]
fn a_waiter_is_woken_by_the_next_buffer_that_comes_back_from_any_thread() {
    let pool = Pool::new(4096);
    let _entered = enter(&pool);
    let held = pool.get(4096).unwrap();
    assert!(pool.get(4096).is_none());
    let (count, w) = waker();
    pool.wait_for_return(w);
    assert_eq!(count.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    std::thread::spawn(move || drop(held)).join().unwrap();
    assert_eq!(
        count.0.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a remote return wakes"
    );

    let held = pool.get(4096).unwrap();
    let (count, w) = waker();
    pool.wait_for_return(w);
    drop(held);
    assert_eq!(
        count.0.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a local return wakes"
    );
}

#[test]
fn a_waiter_is_woken_at_once_when_nothing_is_out_to_come_back() {
    let pool = Pool::new(0);
    let (count, w) = waker();
    pool.wait_for_return(w);
    assert_eq!(count.0.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn a_buffer_that_came_back_before_the_wait_wakes_it_at_once() {
    let pool = Pool::new(4096);
    let _entered = enter(&pool);
    let held = pool.get(4096).unwrap();
    std::thread::spawn(move || drop(held)).join().unwrap();
    let (count, w) = waker();
    pool.wait_for_return(w);
    assert_eq!(count.0.load(std::sync::atomic::Ordering::SeqCst), 1);
}

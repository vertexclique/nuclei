use core::cell::Cell;

use super::BlockRc;

struct Counted<'a>(&'a Cell<usize>);

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

#[test]
fn the_value_is_dropped_once_with_its_last_handle() {
    let drops = Cell::new(0);
    let a = BlockRc::new(Counted(&drops));
    let b = a.clone();
    drop(a);
    assert_eq!(drops.get(), 0);
    drop(b);
    assert_eq!(drops.get(), 1);
}

#[test]
fn handles_share_one_value() {
    let a = BlockRc::new(Cell::new(0u32));
    let b = a.clone();
    b.set(7);
    assert_eq!(a.get(), 7);
}

/// The block of a value dropped goes back to this thread's cache, whose next
/// block of its size it is: a second value takes no memory of the allocator.
#[test]
fn a_dropped_block_is_the_next_one_of_its_size() {
    std::thread::spawn(|| {
        let a = BlockRc::new([0u64; 16]);
        let addr = core::ptr::from_ref(&*a) as usize;
        drop(a);
        let b = BlockRc::new([1u64; 16]);
        assert_eq!(core::ptr::from_ref(&*b) as usize, addr);
    })
    .join()
    .unwrap();
}

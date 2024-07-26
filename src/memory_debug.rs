#![cfg(feature = "memory_debug")]

use backtrace::Backtrace;
use itertools::Itertools;
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Write;
use std::sync::LazyLock as Lazy;

pub struct TraceAllocator {
    trace: Lazy<Mutex<FxHashMap<usize, (usize, Backtrace)>>>,
}

impl TraceAllocator {
    const fn new() -> Self {
        Self {
            trace: Lazy::new(|| Mutex::new(FxHashMap::default())),
        }
    }

    pub fn dump(&self) -> String {
        let mut trace_to_size: FxHashMap<_, (usize, Backtrace)> = FxHashMap::default();
        for (size, t) in self.trace.lock().values() {
            let key = t.frames().iter().map(|a| a.ip() as usize).collect_vec();
            trace_to_size.entry(key).or_insert((0, t.clone())).0 += size;
        }
        let mut trace_to_size = trace_to_size.into_iter().collect_vec();
        trace_to_size.sort_by_key(|(_, (s, _))| *s);
        let mut r = String::new();
        for (n, (_, (size, mut trace))) in trace_to_size.into_iter().rev().take(5).enumerate() {
            trace.resolve();
            writeln!(&mut r, "============== {}, {size} ==============", n + 1).unwrap();
            writeln!(&mut r, "{trace:?}").unwrap();
        }
        r
    }
}

unsafe impl GlobalAlloc for TraceAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc(layout);
        if let Some(mut t) = self.trace.try_lock() {
            t.insert(p as usize, (layout.size(), Backtrace::new_unresolved()));
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if let Some(mut t) = self.trace.try_lock() {
            t.remove(&(ptr as usize));
        }
        System.dealloc(ptr, layout)
    }
}

#[global_allocator]
pub static GLOBAL_ALLOC: TraceAllocator = TraceAllocator::new();

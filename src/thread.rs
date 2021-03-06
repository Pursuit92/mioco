use std;
use std::any::Any;
use std::cell::{RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::panic;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::coroutine::{self, Coroutine, CoroutineSlabHandle, RcCoroutine};
use super::{SchedulerThread, token_to_ids, CoroutineControl};
use super::mio_orig::{self, EventLoop, Token, EventSet};

use slab;
use context::Context;

/// Current coroutine thread-local reference
///
/// This reference is used to store a reference to a currently executing
/// mioco coroutine.
///
/// Should not be used directly, use `tl_coroutine_current()` instead.
thread_local!(pub static TL_CURRENT_COROUTINE: RefCell<*mut Coroutine> = RefCell::new(ptr::null_mut()));

/// Can send `Message` to the mioco thread.
pub type MioSender =
    mio_orig::Sender<<Handler as mio_orig::Handler>::Message>;

pub type RcHandlerShared = Rc<RefCell<HandlerShared>>;
pub type ArcHandlerThreadShared = Arc<HandlerThreadShared>;


pub struct HandlerThreadShared {
    mioco_started: AtomicUsize,
    coroutines_num: AtomicUsize,
    #[allow(dead_code)]
    thread_num: AtomicUsize,
}

impl HandlerThreadShared {
    pub fn new(thread_num: usize) -> Self {
        HandlerThreadShared {
            mioco_started: AtomicUsize::new(0),
            coroutines_num: AtomicUsize::new(0),
            thread_num: AtomicUsize::new(thread_num),
        }
    }
}

/// Data belonging to `Handler`, but referenced and manipulated by coroutinees
/// belonging to it.
pub struct HandlerShared {
    /// Slab allocator
    pub coroutines: slab::Slab<CoroutineSlabHandle, coroutine::Id>,

    /// Context saved when jumping into coroutine
    pub context: Context,

    /// Senders to other EventLoops
    senders: Vec<MioSender>,

    /// Shared between threads
    thread_shared: ArcHandlerThreadShared,

    /// Default stack size
    pub stack_size: usize,

    /// Newly spawned Coroutines
    spawned: Vec<CoroutineControl>,

    /// Coroutines that were made ready
    ready: Vec<CoroutineControl>,

    thread_id: usize,
}

impl HandlerShared {
    pub fn new(senders: Vec<MioSender>,
           thread_shared: ArcHandlerThreadShared,
           stack_size: usize,
           thread_id: usize)
           -> Self {
        HandlerShared {
            coroutines: slab::Slab::new(512),
            thread_shared: thread_shared,
            context: Context::empty(),
            senders: senders,
            stack_size: stack_size,
            spawned: Vec::new(),
            ready: Vec::new(),
            thread_id: thread_id,
        }
    }

    pub fn add_spawned(&mut self, coroutine_ctrl : CoroutineControl) {
        self.spawned.push(coroutine_ctrl);
    }

    pub fn add_ready(&mut self, coroutine_ctrl : CoroutineControl) {
        self.ready.push(coroutine_ctrl);
    }

    pub fn get_sender_to_own_thread(&self) -> MioSender {
        self.senders[self.thread_id].clone()
    }

    pub fn get_sender_to_thread(&self, thread_id : usize) -> MioSender {
        self.senders[thread_id].clone()
    }

    pub fn wait_for_start_all(&self) {
        while self.thread_shared.mioco_started.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now()
        }
    }

    pub fn signal_start_all(&self) {
        self.thread_shared.mioco_started.store(1, Ordering::SeqCst)
    }

    fn coroutines_num(&self) -> usize {
        // Relaxed is OK, since Threads will eventually notice if it goes to
        // zero and at the start `SeqCst` in `mioco_start` and
        // `mioco_started` will enforce that `coroutines_num > 0` is visible
        // on all threads at the start.
        self.thread_shared.coroutines_num.load(Ordering::Relaxed)
    }

    pub fn coroutines_inc(&self) {
        self.thread_shared.coroutines_num.fetch_add(1, Ordering::SeqCst);
    }

    pub fn coroutines_dec(&self) {
        let prev = self.thread_shared.coroutines_num.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(prev > 0);
    }

    /// Get number of threads
    pub fn thread_num(&self) -> usize {
        self.thread_shared.thread_num.load(Ordering::Relaxed)
    }

    pub fn attach(&mut self, rc_coroutine : RcCoroutine) -> coroutine::Id {
        let co_slab_handle = CoroutineSlabHandle::new(rc_coroutine);

        if !self.coroutines.has_remaining() {
            let count = self.coroutines.count();
            self.coroutines.grow(count);
        }

        self.coroutines.insert(co_slab_handle)
            .unwrap_or_else(|_| panic!())
    }

}

/// Mioco event loop `Handler`
///
/// Registered in `mio_orig::EventLoop` and implementing `mio_orig::Handler`.  This `struct` is quite
/// internal so you should not have to worry about it.
pub struct Handler {
    shared: RcHandlerShared,
    scheduler: Box<SchedulerThread + 'static>,
}

impl Handler {
    /// Create a Handler.
    pub fn new(shared: RcHandlerShared, scheduler: Box<SchedulerThread>) -> Self {
        Handler {
            shared: shared,
            scheduler: scheduler,
        }
    }

    /// Data shared between Handler and Coroutines belonging to it
    pub fn shared(&self) -> &RcHandlerShared {
        &self.shared
    }

    /// To prevent recursion, all the newly spawned or newly made
    /// ready Coroutines are delivered to scheduler here.
    pub fn deliver_to_scheduler(&mut self, event_loop: &mut EventLoop<Self>) {
        let Handler {
            ref shared,
            ref mut scheduler,
        } = *self;

        loop {
            let mut spawned = Vec::new();
            let mut ready = Vec::new();
            {
                let mut shared = shared.borrow_mut();

                if shared.spawned.len() == 0 && shared.ready.len() == 0 {
                    break;
                }
                std::mem::swap(&mut spawned, &mut shared.spawned);
                std::mem::swap(&mut ready, &mut shared.ready);
            }

            for spawned in spawned.drain(..) {
                scheduler.spawned(event_loop, spawned);
            }

            for ready in ready.drain(..) {
                scheduler.ready(event_loop, ready);
            }
        }
    }
}

/// EventLoop message type
pub enum Message {
    /// Mailbox notification
    MailboxMsg(Token),
    /// Coroutine migration
    Migration(CoroutineControl),
    /// Coroutine Panicked
    PropagatePanic(Box<Any + Send + 'static>),
}

unsafe impl Send for Message {}

impl mio_orig::Handler for Handler {
    type Timeout = Token;
    type Message = Message;

    fn tick(&mut self, event_loop: &mut mio_orig::EventLoop<Self>) {
        let coroutines_num = self.shared.borrow().coroutines_num();
        trace!("Handler::tick(): coroutines_num = {}", coroutines_num);
        if coroutines_num == 0 {
            trace!("Shutting down EventLoop");
            event_loop.shutdown();
        }
        self.scheduler.tick(event_loop);
        self.deliver_to_scheduler(event_loop);
    }

    fn ready(&mut self,
             event_loop: &mut mio_orig::EventLoop<Handler>,
             token: mio_orig::Token,
             events: mio_orig::EventSet) {
        trace!("Handler::ready({:?}): started", token);
        let (co_id, _) = token_to_ids(token);
        let co = {
            let shared = self.shared.borrow();
            match shared.coroutines.get(co_id).as_ref() {
                Some(&co) => co.clone(),
                None => {
                    trace!("Handler::ready() ignored");
                    return;
                }
            }
        };
        if co.event(event_loop, token, events) {
            self.scheduler.ready(event_loop, co.to_coroutine_control());
        }

        self.deliver_to_scheduler(event_loop);

        trace!("Handler::ready({:?}): finished", token);
    }

    fn notify(&mut self, event_loop: &mut EventLoop<Handler>, msg: Self::Message) {
        match msg {
            Message::MailboxMsg(token) => self.ready(event_loop, token, EventSet::readable()),
            Message::Migration(mut coroutine) => {
                coroutine.reattach_to(event_loop, self);
                self.scheduler.ready(event_loop, coroutine);
                self.deliver_to_scheduler(event_loop);
            }
            Message::PropagatePanic(cause) => panic::propagate(cause),
        }
    }

    fn timeout(&mut self, event_loop: &mut EventLoop<Self>, msg: Self::Timeout) {
        self.ready(event_loop, msg, EventSet::readable());
    }
}

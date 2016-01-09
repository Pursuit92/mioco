use super::{EventedInner, EventedShared, RcEvented, RW, Handler, Evented};
use super::prv::EventedPrv;
use super::mio_orig::{EventLoop, Token, EventSet};
use time::{SteadyTime, Duration};
use std::cell::RefCell;
use std::rc::Rc;

/// A Timer generating event after a given time
///
/// Can be used to block coroutine or to implement timeout for other `EventSource`.
///
/// Create using `MiocoHandle::timeout()`.
///
/// Use `MiocoHandle::select()` to wait for an event, or `read()` to block until
/// done.
pub struct Timer {
    rc: RcEvented<TimerCore>,
}

struct TimerCore {
    timeout: SteadyTime,
}

impl Timer {
    /// Create a new timer
    pub fn new() -> Timer {
        let timer_core = TimerCore { timeout: SteadyTime::now() };
        Timer { rc: RcEvented(Rc::new(RefCell::new(EventedShared::new(timer_core)))) }
    }

    fn is_done(&self) -> bool {
        self.rc.should_resume()
    }
}

impl EventedPrv for Timer {
    type Raw = TimerCore;

    fn shared(&self) -> &RcEvented<TimerCore> {
        &self.rc
    }
}

impl Evented for Timer {}

impl Timer {
    /// Read a timer to block on it until it is done.
    ///
    /// Returns current time
    ///
    /// TODO: Return wakeup time instead
    pub fn read(&mut self) -> SteadyTime {
        loop {
            if let Some(t) = self.try_read() {
                return t;
            }

            self.block_on(RW::read());
        }
    }

    /// Try reading current time (if the timer is done)
    ///
    /// TODO: Return wakeup time instead
    pub fn try_read(&mut self) -> Option<SteadyTime> {
        let done = self.is_done();

        if done {
            Some(SteadyTime::now())
        } else {
            None
        }
    }

    /// Set timeout for the timer
    ///
    /// The timeout counts from the time `set_timeout` is called.
    pub fn set_timeout(&mut self, delay_ms: i64) {
        let mut timer_core = &mut self.rc.0.borrow_mut().io;
        timer_core.timeout = SteadyTime::now() + Duration::milliseconds(delay_ms);
    }

    /// Set timeout for the timer using absolute time.
    pub fn set_timeout_absolute(&mut self, timeout: SteadyTime) {
        let mut timer_core = &mut self.rc.0.borrow_mut().io;
        timer_core.timeout = timeout;
    }


    /// Get absolute value of the timer timeout.
    pub fn get_timeout_absolute(&mut self) -> SteadyTime {
        self.rc.0.borrow().io.timeout
    }
}

impl EventedInner for TimerCore {
    fn register(&self, event_loop: &mut EventLoop<Handler>, token: Token, _interest: EventSet) {
        let timeout = self.timeout;
        let now = SteadyTime::now();
        let delay = if timeout <= now {
            0
        } else {
            (timeout - now).num_milliseconds()
        };

        trace!("Timer({}): set timeout in {}ms", token.as_usize(), delay);
        match event_loop.timeout_ms(token, delay as u64) {
            Ok(_) => {}
            Err(reason) => {
                panic!("Could not create mio::Timeout: {:?}", reason);
            }
        }
    }

    fn reregister(&self, event_loop: &mut EventLoop<Handler>, token: Token, interest: EventSet) {
        self.register(event_loop, token, interest)
    }

    fn deregister(&self, _event_loop: &mut EventLoop<Handler>, _token: Token) {}

    fn should_resume(&self) -> bool {
        trace!("Timer: should_resume? {}",
               self.timeout <= SteadyTime::now());
        self.timeout <= SteadyTime::now()
    }
}

unsafe impl Send for Timer {}

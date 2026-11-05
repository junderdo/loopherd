//! Keeps the herd view subscribed to herdr's events: one lifecycle connection,
//! and one status connection covering the panes the view shows.

use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::herdr::events::{Connection, PaneNotFound, Pushed, Subscriber, Subscription};

const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Live this long before a drop, the backoff starts over; a herdr that keeps
/// accepting then dropping still backs off.
const STABLE: Duration = Duration::from_secs(30);

const LIFECYCLE: [Subscription; 3] = [
    Subscription::PaneCreated,
    Subscription::PaneClosed,
    Subscription::AgentDetected,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Something herdr shows may have changed; read again.
    Invalidated,
    /// Whether the lifecycle connection is up and the status connection
    /// covers the shown panes, so events cover agent status.
    Live(bool),
}

enum Msg {
    Panes(Vec<String>),
    Pushed(u64, Pushed),
    Stop,
}

/// The running watcher's end the view holds.
pub struct Watch {
    inbox: Sender<Msg>,
    pub signals: Receiver<Signal>,
}

impl Watch {
    /// The panes the view now shows, in any order.
    pub fn show(&self, panes: Vec<String>) {
        let _ = self.inbox.send(Msg::Panes(panes));
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.inbox.send(Msg::Stop);
    }
}

/// Starts watching on its own thread, which closes its connections and stops
/// once the [`Watch`] drops.
pub fn watch<S: Subscriber + 'static>(subscriber: S) -> Watch {
    let (signal, signals) = mpsc::channel();
    let watcher = Watcher::new(subscriber, signal, Instant::now());
    let inbox = watcher.outbox.clone();
    std::thread::spawn(move || watcher.run());
    Watch { inbox, signals }
}

struct Open {
    id: u64,
    _connection: Connection,
}

struct Watcher<S> {
    subscriber: S,
    inbox: Receiver<Msg>,
    outbox: Sender<Msg>,
    signals: Sender<Signal>,
    panes: Vec<String>,
    lifecycle: Option<Open>,
    /// With the panes it covers.
    status: Option<(Vec<String>, Open)>,
    next_id: u64,
    /// When the view last went live; `None` while it isn't.
    live_since: Option<Instant>,
    backoff: Duration,
    retry_at: Option<Instant>,
    view_gone: bool,
}

impl<S: Subscriber> Watcher<S> {
    fn new(subscriber: S, signals: Sender<Signal>, now: Instant) -> Watcher<S> {
        let (outbox, inbox) = mpsc::channel();
        Watcher {
            subscriber,
            inbox,
            outbox,
            signals,
            panes: vec![],
            lifecycle: None,
            status: None,
            next_id: 0,
            live_since: None,
            backoff: FIRST_BACKOFF,
            retry_at: Some(now),
            view_gone: false,
        }
    }

    fn run(mut self) {
        while !self.view_gone {
            self.retry_if_due(Instant::now());
            let msg = match self.retry_at {
                Some(at) => self
                    .inbox
                    .recv_timeout(at.saturating_duration_since(Instant::now()))
                    .ok(),
                None => self.inbox.recv().ok(),
            };
            if let Some(msg) = msg {
                self.handle(msg, Instant::now());
            }
        }
    }

    /// Handles what has arrived, then retries subscribing if that is due.
    #[cfg(test)]
    fn pump(&mut self, now: Instant) {
        while let Ok(msg) = self.inbox.try_recv() {
            self.handle(msg, now);
        }
        self.retry_if_due(now);
    }

    fn handle(&mut self, msg: Msg, now: Instant) {
        match msg {
            Msg::Panes(mut panes) => {
                panes.sort();
                panes.dedup();
                self.panes = panes;
                if self.lifecycle.is_some() {
                    self.sync_status(now);
                }
            }
            Msg::Pushed(_, Pushed::Event) => self.signal(Signal::Invalidated),
            Msg::Pushed(id, Pushed::Ended) if self.is_current(id) => self.fail(now),
            // A connection this watcher replaced or closed.
            Msg::Pushed(_, Pushed::Ended) => {}
            Msg::Stop => self.view_gone = true,
        }
    }

    fn is_current(&self, id: u64) -> bool {
        let lifecycle = self.lifecycle.as_ref().map(|o| o.id);
        let status = self.status.as_ref().map(|(_, o)| o.id);
        [lifecycle, status].contains(&Some(id))
    }

    fn retry_if_due(&mut self, now: Instant) {
        if self.retry_at.is_none_or(|at| now < at) {
            return;
        }
        self.retry_at = None;
        if self.lifecycle.is_none() {
            match self.open(&LIFECYCLE) {
                Ok(open) => {
                    self.lifecycle = Some(open);
                    self.signal(Signal::Invalidated);
                }
                Err(_) => return self.fail(now),
            }
        }
        self.sync_status(now);
    }

    /// Replaces the status connection when the panes changed; goes live once
    /// it covers them.
    fn sync_status(&mut self, now: Instant) {
        let covered = self.status.as_ref().map(|(panes, _)| panes);
        if covered != Some(&self.panes) {
            if self.panes.is_empty() {
                self.status = None;
            } else {
                let subscriptions: Vec<Subscription> = self
                    .panes
                    .iter()
                    .cloned()
                    .map(Subscription::AgentStatusChanged)
                    .collect();
                match self.open(&subscriptions) {
                    Ok(open) => self.status = Some((self.panes.clone(), open)),
                    Err(e) if e.is::<PaneNotFound>() => {
                        // A pane closed since the view's read; the fresh read
                        // brings the pane list to subscribe to.
                        self.status = None;
                        self.signal(Signal::Invalidated);
                        return self.fall_back(now);
                    }
                    Err(_) => return self.fail(now),
                }
                self.signal(Signal::Invalidated);
            }
        }
        if self.live_since.is_none() {
            self.live_since = Some(now);
            self.signal(Signal::Live(true));
        }
    }

    fn open(&mut self, subscriptions: &[Subscription]) -> Result<Open> {
        let id = self.next_id;
        self.next_id += 1;
        let outbox = self.outbox.clone();
        let on_push = Box::new(move |pushed| {
            let _ = outbox.send(Msg::Pushed(id, pushed));
        });
        let connection = self.subscriber.subscribe(subscriptions, on_push)?;
        Ok(Open {
            id,
            _connection: connection,
        })
    }

    /// Drops every connection and retries after the backoff.
    fn fail(&mut self, now: Instant) {
        self.lifecycle = None;
        self.status = None;
        self.fall_back(now);
    }

    fn fall_back(&mut self, now: Instant) {
        if let Some(since) = self.live_since.take() {
            if now >= since + STABLE {
                self.backoff = FIRST_BACKOFF;
            }
            self.signal(Signal::Live(false));
        }
        self.retry_at = Some(now + self.backoff);
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
    }

    fn signal(&mut self, signal: Signal) {
        self.view_gone |= self.signals.send(signal).is_err();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::{FakeSubscriber, Refusal, Which};

    fn lifecycle() -> Vec<Subscription> {
        LIFECYCLE.to_vec()
    }

    fn status(panes: &[&str]) -> Vec<Subscription> {
        panes
            .iter()
            .map(|p| Subscription::AgentStatusChanged(p.to_string()))
            .collect()
    }

    fn panes(panes: &[&str]) -> Msg {
        Msg::Panes(panes.iter().map(|p| p.to_string()).collect())
    }

    struct Harness {
        fake: FakeSubscriber,
        watcher: Watcher<FakeSubscriber>,
        signals: Receiver<Signal>,
    }

    impl Harness {
        fn start(t0: Instant) -> Harness {
            let fake = FakeSubscriber::default();
            let (signal, signals) = mpsc::channel();
            let mut watcher = Watcher::new(fake.clone(), signal, t0);
            watcher.pump(t0);
            Harness {
                fake,
                watcher,
                signals,
            }
        }

        fn show(&mut self, shown: &[&str], now: Instant) {
            self.watcher.handle(panes(shown), now);
            self.watcher.pump(now);
        }

        fn signals(&self) -> Vec<Signal> {
            self.signals.try_iter().collect()
        }
    }

    #[test]
    fn subscribes_to_lifecycle_then_to_the_status_of_each_shown_pane() {
        let t0 = Instant::now();
        let mut h = Harness::start(t0);
        assert_eq!(h.fake.open(), [lifecycle()]);
        h.show(&["w1:pb", "w2:p7", "w1:pa"], t0);
        assert_eq!(
            h.fake.open(),
            [lifecycle(), status(&["w1:pa", "w1:pb", "w2:p7"])]
        );
        assert!(h.signals().contains(&Signal::Live(true)));
    }

    #[test]
    fn a_new_pane_set_replaces_the_status_connection_and_stays_live() {
        let t0 = Instant::now();
        let mut h = Harness::start(t0);
        h.show(&["w1:pa"], t0);
        h.signals();
        h.show(&["w1:pa", "w1:pc"], t0);
        assert_eq!(h.fake.open(), [lifecycle(), status(&["w1:pa", "w1:pc"])]);
        h.show(&[], t0);
        assert_eq!(h.fake.open(), [lifecycle()]);
        assert!(!h.signals().contains(&Signal::Live(false)));
    }

    #[test]
    fn the_same_pane_set_keeps_its_connection() {
        let t0 = Instant::now();
        let mut h = Harness::start(t0);
        h.show(&["w1:pa", "w1:pb"], t0);
        h.signals();
        h.show(&["w1:pb", "w1:pa"], t0);
        assert_eq!(h.signals(), []);
        assert_eq!(h.fake.open(), [lifecycle(), status(&["w1:pa", "w1:pb"])]);
    }

    #[test]
    fn any_pushed_event_invalidates_the_view() {
        let t0 = Instant::now();
        let mut h = Harness::start(t0);
        h.show(&["w1:pa"], t0);
        h.signals();
        for which in [Which::Status, Which::Lifecycle] {
            h.fake.push(which);
            h.watcher.pump(t0);
            assert_eq!(h.signals(), [Signal::Invalidated], "{which:?}");
        }
    }

    #[test]
    fn pane_not_found_keeps_lifecycle_and_resubscribes_with_the_refreshed_panes() {
        let t0 = Instant::now();
        let mut h = Harness::start(t0);
        h.signals();
        h.fake.refuse_next(Refusal::PaneNotFound);
        h.show(&["w1:pa", "w1:pgone"], t0);
        assert_eq!(h.signals(), [Signal::Invalidated, Signal::Live(false)]);
        assert_eq!(h.fake.open(), [lifecycle()]);
        h.show(&["w1:pa"], t0 + Duration::from_millis(100));
        assert_eq!(h.fake.open(), [lifecycle(), status(&["w1:pa"])]);
        assert_eq!(h.signals(), [Signal::Invalidated, Signal::Live(true)]);
    }

    #[test]
    fn events_lost_or_a_dropped_connection_falls_back_and_resubscribes() {
        let t0 = Instant::now();
        let mut h = Harness::start(t0);
        h.show(&["w1:pa"], t0);
        let mut now = t0;
        for (which, backoff) in [(Which::Lifecycle, 1), (Which::Status, 2)] {
            h.signals();
            h.fake.end(which);
            h.watcher.pump(now);
            assert_eq!(h.signals(), [Signal::Live(false)], "{which:?}");
            assert_eq!(h.fake.open(), Vec::<Vec<Subscription>>::new());
            now += Duration::from_secs(backoff);
            h.watcher.pump(now);
            assert_eq!(h.fake.open(), [lifecycle(), status(&["w1:pa"])]);
            assert_eq!(
                h.signals(),
                [Signal::Invalidated, Signal::Invalidated, Signal::Live(true)]
            );
        }
    }

    #[test]
    fn failed_subscribes_back_off_from_1s_doubling_to_30s() {
        let t0 = Instant::now();
        let fake = FakeSubscriber::default();
        for _ in 0..7 {
            fake.refuse_next(Refusal::Failed);
        }
        let (signal, _signals) = mpsc::channel();
        let mut watcher = Watcher::new(fake.clone(), signal, t0);
        watcher.pump(t0);
        let mut now = t0;
        for wait in [1, 2, 4, 8, 16, 30, 30] {
            now = retried_after(&mut watcher, &fake, now, wait);
        }
        assert_eq!(fake.open(), [lifecycle()]);
    }

    #[test]
    fn a_herdr_that_accepts_then_drops_still_backs_off_until_it_stays_up() {
        let t0 = Instant::now();
        let mut h = Harness::start(t0);
        let mut now = t0;
        for wait in [1, 2, 4] {
            h.fake.end(Which::Lifecycle);
            h.watcher.pump(now);
            now = retried_after(&mut h.watcher, &h.fake, now, wait);
        }
        now += STABLE;
        h.fake.end(Which::Lifecycle);
        h.watcher.pump(now);
        retried_after(&mut h.watcher, &h.fake, now, 1);
    }

    /// Checks the watcher next subscribes `secs` after `from`, not before;
    /// returns that time.
    fn retried_after(
        watcher: &mut Watcher<FakeSubscriber>,
        fake: &FakeSubscriber,
        from: Instant,
        secs: u64,
    ) -> Instant {
        let at = from + Duration::from_secs(secs);
        let attempts = fake.attempts();
        watcher.pump(at - Duration::from_millis(1));
        assert_eq!(fake.attempts(), attempts, "retried before {secs}s");
        watcher.pump(at);
        assert_eq!(fake.attempts(), attempts + 1, "no retry at {secs}s");
        at
    }

    #[test]
    fn dropping_the_watch_closes_its_connections() {
        let fake = FakeSubscriber::default();
        let watch = watch(fake.clone());
        let deadline = Instant::now() + Duration::from_secs(5);
        while fake.open().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(fake.open(), [lifecycle()]);
        drop(watch);
        while !fake.open().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(fake.open(), Vec::<Vec<Subscription>>::new());
    }
}

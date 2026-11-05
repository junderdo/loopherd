//! herdr's `events.subscribe`, over the socket [`super::Herdr::socket`] names.
//! Uses only std, serde_json and anyhow, so the live tests can include it.

use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// How long herdr gets to acknowledge a subscription.
const ACK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subscription {
    PaneCreated,
    PaneClosed,
    AgentDetected,
    /// herdr has no global status subscription: one entry per pane.
    AgentStatusChanged(String),
}

impl Subscription {
    fn json(&self) -> Value {
        match self {
            Subscription::PaneCreated => json!({"type": "pane.created"}),
            Subscription::PaneClosed => json!({"type": "pane.closed"}),
            Subscription::AgentDetected => json!({"type": "pane.agent_detected"}),
            Subscription::AgentStatusChanged(pane) => {
                json!({"type": "pane.agent_status_changed", "pane_id": pane})
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushed {
    /// Any pushed event, whatever its kind.
    Event,
    /// The connection is over: closed, dropped, or herdr lost events.
    Ended,
}

/// A subscribed pane doesn't exist, which rejects the whole request. Find it
/// with `anyhow::Error::is`.
#[derive(Debug)]
pub struct PaneNotFound;

impl std::fmt::Display for PaneNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("herdr has no pane by a subscribed id")
    }
}

impl std::error::Error for PaneNotFound {}

/// An open subscription; dropping it closes the connection.
pub struct Connection(Option<Box<dyn FnOnce() + Send>>);

impl Connection {
    pub fn new(close: impl FnOnce() + Send + 'static) -> Connection {
        Connection(Some(Box::new(close)))
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(close) = self.0.take() {
            close();
        }
    }
}

pub type OnPush = Box<dyn Fn(Pushed) + Send + Sync>;

pub trait Subscriber: Send {
    /// Opens a connection for `subscriptions` and returns once herdr has
    /// started it. `on_push` then gets each pushed event, and [`Pushed::Ended`]
    /// once the connection ends, including when it is closed by dropping.
    fn subscribe(&self, subscriptions: &[Subscription], on_push: OnPush) -> Result<Connection>;
}

pub struct SocketSubscriber {
    socket: Option<PathBuf>,
}

impl SocketSubscriber {
    /// `None` when herdr couldn't name its socket; every subscribe then fails.
    pub fn new(socket: Option<PathBuf>) -> SocketSubscriber {
        SocketSubscriber { socket }
    }
}

impl Subscriber for SocketSubscriber {
    fn subscribe(&self, subscriptions: &[Subscription], on_push: OnPush) -> Result<Connection> {
        let socket = self.socket.as_ref().context("herdr named no socket")?;
        let (stream, reader) = start(socket, subscriptions)?;
        std::thread::spawn(move || {
            // Every line after the acknowledgment is an event, except the
            // error (`events_lost`) herdr sends before closing.
            for line in reader.lines() {
                match line.map(|l| parse(&l)) {
                    Ok(Line::Error(_)) | Err(_) => break,
                    Ok(_) => on_push(Pushed::Event),
                }
            }
            on_push(Pushed::Ended);
        });
        Ok(Connection::new(move || {
            let _ = stream.shutdown(Shutdown::Both);
        }))
    }
}

fn start(
    socket: &Path,
    subscriptions: &[Subscription],
) -> Result<(UnixStream, BufReader<UnixStream>)> {
    let mut stream =
        UnixStream::connect(socket).with_context(|| format!("connect to {}", socket.display()))?;
    let request = json!({
        "id": "loopherd",
        "method": "events.subscribe",
        "params": {"subscriptions": subscriptions.iter().map(Subscription::json).collect::<Vec<_>>()},
    });
    writeln!(stream, "{request}")
        .and_then(|()| stream.set_read_timeout(Some(ACK_TIMEOUT)))
        .context("send events.subscribe")?;
    let mut reader = BufReader::new(stream.try_clone().context("clone socket")?);
    let mut ack = String::new();
    reader
        .read_line(&mut ack)
        .context("read the events.subscribe reply")?;
    match parse(&ack) {
        Line::Started => {}
        Line::Error(code) if code == "pane_not_found" => bail!(PaneNotFound),
        Line::Error(code) => bail!("herdr refused events.subscribe: {code}"),
        _ => bail!("unexpected events.subscribe reply: {}", ack.trim()),
    }
    stream
        .set_read_timeout(None)
        .context("clear the read timeout")?;
    Ok((stream, reader))
}

#[derive(Debug, PartialEq, Eq)]
enum Line {
    Started,
    Event,
    Error(String),
    Other,
}

/// herdr spells event kinds both `pane.x` and `pane_x`; any kind is an event.
fn parse(line: &str) -> Line {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Line::Other;
    };
    if let Some(code) = value["error"]["code"].as_str() {
        return Line::Error(code.into());
    }
    if value["result"]["type"] == "subscription_started" {
        Line::Started
    } else if value["event"].is_string() {
        Line::Event
    } else {
        Line::Other
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc::{self, Receiver};

    use super::*;

    const STARTED: &str = r#"{"id":"loopherd","result":{"type":"subscription_started"}}"#;

    /// A one-connection stand-in for herdr: replies with `lines`, then holds
    /// the connection open until the client closes it. Yields the request line,
    /// then `"closed"` once the client hangs up.
    fn server(lines: &'static [&'static str]) -> (tempfile::TempDir, PathBuf, Receiver<String>) {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (seen, saw) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            seen.send(request).unwrap();
            for line in lines {
                writeln!(stream, "{line}").unwrap();
            }
            let mut rest = String::new();
            while reader.read_line(&mut rest).unwrap_or(0) > 0 {}
            let _ = seen.send("closed".into());
        });
        (dir, socket, saw)
    }

    fn pushes() -> (OnPush, Receiver<Pushed>) {
        let (tx, rx) = mpsc::channel();
        (Box::new(move |p| tx.send(p).unwrap()), rx)
    }

    fn next(rx: &Receiver<Pushed>) -> Pushed {
        rx.recv_timeout(Duration::from_secs(5)).unwrap()
    }

    #[test]
    fn sends_one_status_entry_per_pane_alongside_lifecycle_entries() {
        let (_dir, socket, saw) = server(&[STARTED]);
        let (on_push, _rx) = pushes();
        let subscriptions = [
            Subscription::PaneCreated,
            Subscription::PaneClosed,
            Subscription::AgentDetected,
            Subscription::AgentStatusChanged("w1:p1".into()),
        ];
        let _connection = SocketSubscriber::new(Some(socket))
            .subscribe(&subscriptions, on_push)
            .unwrap();
        let request: Value = serde_json::from_str(&saw.recv().unwrap()).unwrap();
        assert_eq!(
            request,
            json!({"id": "loopherd", "method": "events.subscribe", "params": {"subscriptions": [
                {"type": "pane.created"},
                {"type": "pane.closed"},
                {"type": "pane.agent_detected"},
                {"type": "pane.agent_status_changed", "pane_id": "w1:p1"},
            ]}})
        );
    }

    #[test]
    fn every_event_kind_in_either_spelling_is_pushed_and_events_lost_ends_it() {
        let (_dir, socket, _saw) = server(&[
            STARTED,
            r#"{"data":{"agent_status":"working","pane_id":"w1:p1","workspace_id":"w1"},"event":"pane.agent_status_changed"}"#,
            r#"{"data":{"pane_id":"w1:p1","type":"pane_agent_detected","workspace_id":"w1"},"event":"pane_agent_detected"}"#,
            r#"{"data":{"pane_id":"w1:p3","type":"pane_closed"},"event":"pane.closed"}"#,
            r#"{"data":{},"event":"pane_something_new"}"#,
            r#"{"error":{"code":"events_lost","message":"subscriber fell behind"}}"#,
        ]);
        let (on_push, rx) = pushes();
        let _connection = SocketSubscriber::new(Some(socket))
            .subscribe(&[Subscription::PaneCreated], on_push)
            .unwrap();
        let got: Vec<Pushed> = (0..5).map(|_| next(&rx)).collect();
        assert_eq!(
            got,
            [
                Pushed::Event,
                Pushed::Event,
                Pushed::Event,
                Pushed::Event,
                Pushed::Ended
            ]
        );
    }

    #[test]
    fn an_unknown_pane_is_refused_as_pane_not_found() {
        let (_dir, socket, _saw) =
            server(&[r#"{"error":{"code":"pane_not_found","message":"pane w9:p9 not found"}}"#]);
        let (on_push, _rx) = pushes();
        let refused = SocketSubscriber::new(Some(socket))
            .subscribe(&[Subscription::AgentStatusChanged("w9:p9".into())], on_push);
        assert!(refused.err().unwrap().is::<PaneNotFound>());
    }

    #[test]
    fn a_missing_socket_fails() {
        let (on_push, _rx) = pushes();
        let dir = tempfile::tempdir().unwrap();
        let missing = SocketSubscriber::new(Some(dir.path().join("none.sock")))
            .subscribe(&[Subscription::PaneCreated], on_push);
        assert!(missing.is_err());
        let (on_push, _rx) = pushes();
        let unnamed = SocketSubscriber::new(None).subscribe(&[Subscription::PaneCreated], on_push);
        assert!(unnamed.is_err());
    }

    #[test]
    fn dropping_the_connection_closes_it_and_ends_the_pushes() {
        let (_dir, socket, saw) = server(&[STARTED]);
        let (on_push, rx) = pushes();
        let connection = SocketSubscriber::new(Some(socket))
            .subscribe(&[Subscription::PaneCreated], on_push)
            .unwrap();
        saw.recv().unwrap();
        drop(connection);
        assert_eq!(saw.recv_timeout(Duration::from_secs(5)).unwrap(), "closed");
        assert_eq!(next(&rx), Pushed::Ended);
    }

    #[test]
    fn herdr_hanging_up_ends_the_pushes() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut String::new())
                .unwrap();
            writeln!(stream, "{STARTED}").unwrap();
        });
        let (on_push, rx) = pushes();
        let _connection = SocketSubscriber::new(Some(socket))
            .subscribe(&[Subscription::PaneCreated], on_push)
            .unwrap();
        assert_eq!(next(&rx), Pushed::Ended);
    }
}

use std::io::{ErrorKind, Write};
use std::process::ChildStdin;

/// A hook may exit before reading stdin; its exit status is what the test checks.
pub fn feed_stdin(mut stdin: ChildStdin, input: &[u8]) {
    match stdin.write_all(input) {
        Err(e) if e.kind() == ErrorKind::BrokenPipe => {}
        result => result.unwrap(),
    }
}

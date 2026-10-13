//! Output geometry from niri IPC ($NIRI_SOCKET). Other compositors give None.

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// The logical position and size of an output.
#[derive(Clone, Copy, Debug)]
pub struct Logical {
    pub x: i32,
    pub y: i32,
    pub width: i32,
}

/// Sends each request on one connection and returns the `Ok` payload of each reply.
fn requests(reqs: &[&str]) -> Option<Vec<Value>> {
    let mut stream = UnixStream::connect(std::env::var_os("NIRI_SOCKET")?).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut out = Vec::new();
    for r in reqs {
        writeln!(stream, "\"{r}\"").ok()?;
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let mut reply: Value = serde_json::from_str(&line).ok()?;
        out.push(reply.get_mut("Ok")?.take());
    }
    Some(out)
}

pub fn output_logical(name: &str) -> Option<Logical> {
    let r = requests(&["Outputs"])?;
    let l = &r[0]["Outputs"].get(name)?["logical"];
    let n = |k: &str| l.get(k).and_then(Value::as_i64).map(|v| v as i32);
    Some(Logical {
        x: n("x")?,
        y: n("y")?,
        width: n("width")?,
    })
}

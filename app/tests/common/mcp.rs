use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::Value;

pub struct McpServerProcess {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<String>,
    pub log_path: Option<PathBuf>,
}

impl McpServerProcess {
    pub fn spawn(root: &Path) -> Self {
        Self::spawn_with_log(root, None)
    }

    pub fn spawn_with_log(root: &Path, log_path: Option<PathBuf>) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sf"));
        cmd.arg("server")
            .arg("--root")
            .arg(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        if let Some(path) = log_path.as_ref() {
            cmd.env("SOURCE_FAST_LOG_PATH", path);
        }

        let mut child = cmd.spawn().expect("Failed to start sf server");

        let stdin = child.stdin.take().expect("Failed to take stdin");
        let stdout = child.stdout.take().expect("Failed to take stdout");

        let (tx, rx) = mpsc::channel::<String>();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let _ = tx.send(line);
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            child,
            stdin,
            rx,
            log_path,
        }
    }

    pub fn send_line(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("Failed to write to server stdin");
        self.stdin.flush().expect("Failed to flush server stdin");
    }

    pub fn recv_json(&mut self, timeout: Duration) -> Option<Value> {
        let line = self.rx.recv_timeout(timeout).ok()?;
        serde_json::from_str::<Value>(line.trim()).ok()
    }

    pub fn initialize(&mut self) -> Value {
        let init_request = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#;
        self.send_line(init_request);
        let resp = self
            .recv_json(Duration::from_secs(5))
            .expect("No initialize response from server");

        // MCP requires a `notifications/initialized` notification after successful initialize.
        let initialized = r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
        self.send_line(initialized);

        resp
    }

    pub fn call_search_code(&mut self, id: u64, query: &str, file_regex: Option<&str>) -> Value {
        let args = match file_regex {
            Some(re) => format!(
                r#"{{"query":{},"file_regex":{}}}"#,
                serde_json::to_string(query).unwrap(),
                serde_json::to_string(re).unwrap()
            ),
            None => format!(r#"{{"query":{}}}"#, serde_json::to_string(query).unwrap()),
        };

        let req = format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"search_code","arguments":{args}}}}}"#
        );
        self.send_line(&req);

        let deadline = Duration::from_secs(10);
        let start = std::time::Instant::now();
        loop {
            let remaining = deadline.saturating_sub(start.elapsed());
            let Some(msg) = self.recv_json(remaining) else {
                panic!("Timed out waiting for tools/call response");
            };
            if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                return msg;
            }
        }
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for McpServerProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

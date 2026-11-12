//! 单次评论起草的按段只读 MCP；文档身份和凭据由运行时持有。

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::unix::{
    fs::PermissionsExt,
    net::{UnixListener, UnixStream},
};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

const MAX_REQUEST: usize = 8192;
pub(crate) const MAX_CHARS: usize = 2000;
type Check = Box<dyn Fn() -> Result<()> + Send + Sync + 'static>;
type Load = Box<dyn Fn() -> Result<String> + Send + Sync + 'static>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_limit() -> usize {
    1200
}

pub(crate) struct ReaderServer {
    pub name: String,
    path: PathBuf,
    stopped: Arc<AtomicBool>,
    active: Arc<Mutex<std::collections::HashMap<usize, UnixStream>>>,
}

impl ReaderServer {
    pub(crate) fn start(dir: &Path, check: Check, load: Load) -> Result<Self> {
        let path = dir.join("read.sock");
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let stopped = Arc::new(AtomicBool::new(false));
        let active = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let server = Self {
            name: format!("doc_{}", uuid::Uuid::new_v4().simple()),
            path,
            stopped: stopped.clone(),
            active: active.clone(),
        };
        let operation_stop = stopped.clone();
        let check: Arc<dyn Fn() -> Result<()> + Send + Sync> = Arc::new(move || {
            anyhow::ensure!(
                !operation_stop.load(Ordering::Acquire),
                summon_i18n::format!("r5e97a6d7be")
            );
            check()
        });
        let load: Arc<dyn Fn() -> Result<String> + Send + Sync> = Arc::from(load);
        std::thread::spawn(move || {
            let mut next_id = 0usize;
            while !stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let Ok(mut connections) = active.lock() else {
                            break;
                        };
                        if stopped.load(Ordering::Acquire) || connections.len() >= 4 {
                            continue;
                        }
                        let Ok(handle) = stream.try_clone() else {
                            continue;
                        };
                        let id = next_id;
                        next_id = next_id.wrapping_add(1);
                        connections.insert(id, handle);
                        drop(connections);
                        let (active, stopped, check, load) =
                            (active.clone(), stopped.clone(), check.clone(), load.clone());
                        std::thread::spawn(move || {
                            let _ =
                                serve(stream, &stopped, check.as_ref(), load.as_ref(), &mut None);
                            if let Ok(mut connections) = active.lock() {
                                connections.remove(&id);
                            }
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(server)
    }

    pub(crate) fn server_config(&self) -> Result<Value> {
        Ok(
            json!({"command":std::env::current_exe()?,"args":["comment-reader","--socket",self.path],
            "enabled":true,"enabled_tools":["read_document"]}),
        )
    }

    pub(crate) fn claude_config(&self) -> Result<String> {
        let mut config = self.server_config()?;
        config.as_object_mut().unwrap().remove("enabled");
        config.as_object_mut().unwrap().remove("enabled_tools");
        Ok(json!({"mcpServers":{&self.name:config}}).to_string())
    }
}

impl Drop for ReaderServer {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Ok(mut slot) = self.active.lock() {
            for (_, stream) in slot.drain() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn page(
    args: Value,
    check: &(dyn Fn() -> Result<()> + Send + Sync),
    load: &(dyn Fn() -> Result<String> + Send + Sync),
    cache: &mut Option<Vec<char>>,
) -> Result<Value> {
    let args: Page = serde_json::from_value(args)?;
    anyhow::ensure!(
        (1..=MAX_CHARS).contains(&args.limit),
        summon_i18n::format!("r59c5a8c267")
    );
    check()?;
    if cache.is_none() {
        let body = load()?;
        check()?;
        *cache = Some(body.chars().collect());
    }
    let body = cache.as_ref().unwrap();
    anyhow::ensure!(
        args.offset <= body.len(),
        summon_i18n::format!("r242f76c8ed")
    );
    let end = args.offset.saturating_add(args.limit).min(body.len());
    Ok(
        json!({"text":body[args.offset..end].iter().collect::<String>(),
        "offset":args.offset,"next_offset":(end<body.len()).then_some(end),"total_chars":body.len()}),
    )
}

fn response(
    message: &Value,
    check: &(dyn Fn() -> Result<()> + Send + Sync),
    load: &(dyn Fn() -> Result<String> + Send + Sync),
    cache: &mut Option<Vec<char>>,
) -> Value {
    let id = &message["id"];
    let result = match message["method"].as_str() {
        Some("initialize") => json!({"protocolVersion":message["params"]["protocolVersion"],
            "capabilities":{"tools":{}},"serverInfo":{"name":"comment-document","version":crate::version::VERSION}}),
        Some("ping") => json!({}),
        Some("tools/list") => json!({"tools":[{"name":"read_document",
            "description":summon_i18n::text!("r8cb977701c"),
            "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false},
            "inputSchema":{"type":"object","properties":{"offset":{"type":"integer","minimum":0},
                "limit":{"type":"integer","minimum":1,"maximum":MAX_CHARS}},"additionalProperties":false}}]}),
        Some("tools/call") if message["params"]["name"] == "read_document" => {
            let args = message["params"]
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match page(args, check, load, cache) {
                Ok(body) => json!({"content":[{"type":"text","text":body.to_string()}]}),
                Err(error) => {
                    json!({"isError":true,"content":[{"type":"text","text":summon_i18n::format!("re283625d63")}]})
                }
            }
        }
        _ => {
            return json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":summon_i18n::text!("r843b785b12")}})
        }
    };
    json!({"jsonrpc":"2.0","id":id,"result":result})
}

fn serve(
    mut stream: UnixStream,
    stopped: &AtomicBool,
    check: &(dyn Fn() -> Result<()> + Send + Sync),
    load: &(dyn Fn() -> Result<String> + Send + Sync),
    cache: &mut Option<Vec<char>>,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut pending = Vec::new();
    let mut buf = [0; 1024];
    while !stopped.load(Ordering::Acquire) {
        let count = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(error) => return Err(error.into()),
        };
        for byte in &buf[..count] {
            pending.push(*byte);
            anyhow::ensure!(
                pending.len() <= MAX_REQUEST,
                summon_i18n::format!("rca67d22033")
            );
            if *byte != b'\n' {
                continue;
            }
            let message: Value = serde_json::from_slice(&pending)?;
            pending.clear();
            if message.get("id").is_none() {
                continue;
            }
            let reply = response(&message, check, load, cache);
            anyhow::ensure!(
                !stopped.load(Ordering::Acquire),
                summon_i18n::format!("re4a5159798")
            );
            serde_json::to_writer(&mut stream, &reply)?;
            stream.write_all(b"\n")?;
        }
    }
    Ok(())
}

/// 子进程只转发 MCP 数据；目标文档、读取权限和 API 凭据留在起草运行时。
pub(crate) fn proxy(socket: &Path) -> Result<()> {
    let mut stream = UnixStream::connect(socket).context(summon_i18n::text!("r5e97a6d7be"))?;
    let mut input = stream.try_clone()?;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin(), &mut input);
        let _ = input.shutdown(std::net::Shutdown::Write);
    });
    std::io::copy(&mut stream, &mut std::io::stdout())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::sync::atomic::AtomicUsize;

    #[test]
    #[ignore = "needs SUMMON_TEST_READER_BIN; executes the built stdio MCP proxy without model calls"]
    fn native_document_reader_stdio_proxy() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        let bin = std::env::var_os("SUMMON_TEST_READER_BIN").expect("SUMMON_TEST_READER_BIN");
        let dir = tempfile::tempdir().unwrap();
        let server = ReaderServer::start(
            dir.path(),
            Box::new(|| Ok(())),
            Box::new(|| Ok("甲乙丙丁".into())),
        )
        .unwrap();
        let mut child = Command::new(bin)
            .args(["comment-reader", "--socket"])
            .arg(&server.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let killer = crate::runtime::executor::ChildKiller::new(child.id() as i32);
        let mut input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let _ = tx.send(line.unwrap());
            }
        });
        for request in [
            json!({"id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}),
            json!({"id":2,"method":"tools/list"}),
            json!({"id":3,"method":"tools/call","params":{"name":"read_document","arguments":{"offset":1,"limit":2}}}),
        ] {
            serde_json::to_writer(&mut input, &request).unwrap();
            input.write_all(b"\n").unwrap();
            input.flush().unwrap();
            let result = rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap_or_else(|error| {
                    killer.kill();
                    panic!("stdio MCP: {error}")
                });
            let result: Value = serde_json::from_str(&result).unwrap();
            assert!(result.get("error").is_none());
            if request["id"] == 3 {
                assert!(result["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("乙丙"));
            }
        }
        drop(server);
        drop(input);
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn document_reader_is_lazy_paged_and_revalidates_cached_content() {
        let calls = Arc::new(AtomicUsize::new(0));
        let allowed = Arc::new(AtomicBool::new(true));
        let permit = allowed.clone();
        let check: Check = Box::new(move || {
            anyhow::ensure!(permit.load(Ordering::SeqCst), "权限已变化");
            Ok(())
        });
        let count = calls.clone();
        let load: Load = Box::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            Ok("甲乙丙丁戊".into())
        });
        let mut cache = None;
        let tools = response(
            &json!({"id":1,"method":"tools/list"}),
            &check,
            &load,
            &mut cache,
        );
        assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let first = page(json!({"offset":0,"limit":2}), &check, &load, &mut cache).unwrap();
        assert_eq!(first["text"], "甲乙");
        assert_eq!(first["next_offset"], 2);
        assert_eq!(
            page(json!({"offset":2,"limit":2}), &check, &load, &mut cache).unwrap()["text"],
            "丙丁"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        for args in [
            json!({"url":"https://other.invalid"}),
            json!({"command":"write"}),
            json!({"limit":2001}),
            json!({"offset":-1}),
        ] {
            assert!(page(args, &check, &load, &mut cache).is_err());
        }
        allowed.store(false, Ordering::SeqCst);
        assert!(page(json!({}), &check, &load, &mut cache).is_err());
    }

    #[test]
    fn document_reader_socket_and_capability_close_with_the_draft() {
        use std::io::{BufRead, BufReader};
        let dir = tempfile::tempdir().unwrap();
        let server = ReaderServer::start(
            dir.path(),
            Box::new(|| Ok(())),
            Box::new(|| Ok("body".into())),
        )
        .unwrap();
        let mut socket = UnixStream::connect(&server.path).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        socket.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"read_document\",\"arguments\":{\"limit\":2}}}\n").unwrap();
        let mut reader = BufReader::new(socket);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.contains("bo"));
        let mut control = UnixStream::connect(&server.path).unwrap();
        control
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        control
            .write_all(b"{\"id\":2,\"method\":\"tools/list\"}\n")
            .unwrap();
        let mut control = BufReader::new(control);
        let mut tools = String::new();
        control.read_line(&mut tools).unwrap();
        assert!(tools.contains("read_document"));
        let path = server.path.clone();
        drop(server);
        assert!(!path.exists());
        line.clear();
        assert_eq!(reader.read_line(&mut line).unwrap(), 0);
        tools.clear();
        assert_eq!(control.read_line(&mut tools).unwrap(), 0);
    }
}

//! P7.3 (C1) — IPC 控制面：Unix socket 监听线程 + mpsc 回主循环。
//!
//! 架构（IMPL-2，三审采纳）：独立线程 + mpsc channel，与 hot-reload 一致——
//! 主循环保持单线程可变状态所有权（tracker 不加额外锁）。
//!
//! 协议（文本行，root-only 0750）：
//!   请求：status | dump <pid> | reload | apply <pid>
//!   响应：多行文本（status/dump 的字段行；命令确认行）
//!
//! 安全：socket 文件 0750 root-only；命令白名单（不做任意命令执行）。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::sync::mpsc::Sender;
use std::thread;

/// 主循环可处理的 IPC 请求。
pub enum IpcRequest {
    Status,
    Dump(i32),
    Reload,
    Apply(i32),
}

/// 启动 IPC 监听线程。返回 (请求 rx 给主循环, 线程句柄)。
/// `path` 为 socket 路径；已存在时先移除（旧 socket 残留）。
pub fn spawn_ipc_server(
    path: &str,
    tx: Sender<(IpcRequest, Sender<String>)>,
) -> std::io::Result<thread::JoinHandle<()>> {
    let socket_path = path.to_string();
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;
    // root-only 0750（与规划书 C1 一致）
    let _ = std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o750));

    Ok(thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else {
                continue;
            };
            // 单连接串行处理（CLI 低频；status/dump 秒级响应）
            let mut reader = BufReader::new(stream.try_clone().expect("socket clone"));
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let line = line.trim();
            let req = parse_request(line);
            let (reply_tx, reply_rx) = std::sync::mpsc::channel();
            if tx.send((req, reply_tx)).is_err() {
                continue; // 主循环已退出
            }
            if let Ok(resp) = reply_rx.recv() {
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        }
        // 监听线程退出：清理 socket 文件
        let _ = std::fs::remove_file(&socket_path);
    }))
}
/// 文本行 → 请求（白名单解析）。
fn parse_request(line: &str) -> IpcRequest {
    let mut parts = line.split_whitespace();
    match parts.next() {
        Some("status") => IpcRequest::Status,
        Some("dump") => parts
            .next()
            .and_then(|p| p.parse::<i32>().ok())
            .map(IpcRequest::Dump)
            .unwrap_or(IpcRequest::Status),
        Some("reload") => IpcRequest::Reload,
        Some("apply") => parts
            .next()
            .and_then(|p| p.parse::<i32>().ok())
            .map(IpcRequest::Apply)
            .unwrap_or(IpcRequest::Status),
        _ => IpcRequest::Status, // 未知命令 → status（不执行任意命令）
    }
}

/// CLI 侧：连接 daemon 发送命令并打印响应（退出码 0/1）。
pub fn cli_command(socket_path: &str, cmd: &str) -> i32 {
    use std::os::unix::net::UnixStream;
    let mut stream = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: 无法连接 daemon IPC ({socket_path}): {e}——daemon 是否在运行？");
            return 1;
        }
    };
    if let Err(e) = writeln!(stream, "{cmd}") {
        eprintln!("error: 发送命令失败: {e}");
        return 1;
    }
    let _ = stream.flush();
    let mut resp = String::new();
    use std::io::Read;
    if stream.read_to_string(&mut resp).is_ok() {
        print!("{resp}");
    }
    0
}

// 测试用：确保 parse_request 白名单正确
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status() {
        assert!(matches!(parse_request("status"), IpcRequest::Status));
    }

    #[test]
    fn parse_dump_with_pid() {
        match parse_request("dump 1234") {
            IpcRequest::Dump(pid) => assert_eq!(pid, 1234),
            _ => panic!("dump 解析错误"),
        }
    }

    #[test]
    fn parse_reload() {
        assert!(matches!(parse_request("reload"), IpcRequest::Reload));
    }

    #[test]
    fn parse_apply_with_pid() {
        match parse_request("apply 5678") {
            IpcRequest::Apply(pid) => assert_eq!(pid, 5678),
            _ => panic!("apply 解析错误"),
        }
    }

    #[test]
    fn parse_unknown_falls_back_to_status() {
        assert!(matches!(parse_request("rm -rf /"), IpcRequest::Status), "未知命令必须回退 status（白名单）");
        assert!(matches!(parse_request(""), IpcRequest::Status));
        assert!(matches!(parse_request("dump"), IpcRequest::Status), "dump 无 pid 回退");
    }
}

//! 分片传输 —— 节点之间直接用 TCP 传分片数据（阶段二）。
//!
//! 每个客户端后台跑一个"分片服务"，监听随机端口，响应其他节点的分片拉取。
//! 下载方用 `fetch_chunk` 主动连接持有者拉取。
//!
//! 线协议（二进制）：
//!   请求:  [u16 be: hash 长度][hash utf8 bytes]
//!   响应:  [u32 be: chunk 长度][chunk bytes]     （长度为 0 表示对方没有此分片）

use crate::chunk::ChunkStore;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

/// 处理一个分片拉取连接：读 hash，回分片字节。
fn serve_conn(mut stream: TcpStream, store: Arc<ChunkStore>) {
    // 读 hash 长度（u16 be）。
    let mut len_buf = [0u8; 2];
    if stream.read_exact(&mut len_buf).is_err() {
        return;
    }
    let hash_len = u16::from_be_bytes(len_buf) as usize;
    if hash_len == 0 || hash_len > 128 {
        return; // sha256 hex 是 64 字符，给点余量，异常长度直接拒绝
    }

    let mut hash_buf = vec![0u8; hash_len];
    if stream.read_exact(&mut hash_buf).is_err() {
        return;
    }
    let hash = match String::from_utf8(hash_buf) {
        Ok(h) => h,
        Err(_) => return,
    };

    // 查本地 store，回分片（无则回长度 0）。
    let data = store.get(&hash).unwrap_or_default();
    let out_len = (data.len() as u32).to_be_bytes();
    let _ = stream.write_all(&out_len);
    if !data.is_empty() {
        let _ = stream.write_all(&data);
    }
    let _ = stream.flush();
}

/// 启动分片服务，绑定到随机端口，返回实际监听地址（"127.0.0.1:xxxxx"）。
/// 后台线程持续 accept，不阻塞调用方。
pub fn start_chunk_server(store: Arc<ChunkStore>) -> std::io::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?.to_string();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    let store = Arc::clone(&store);
                    std::thread::spawn(move || serve_conn(s, store));
                }
                Err(e) => eprintln!("[transfer] accept error: {e}"),
            }
        }
    });

    Ok(addr)
}

/// 从指定 peer 地址拉取一个分片。返回分片字节；对方没有或出错则 Err。
pub fn fetch_chunk(peer_addr: &str, hash: &str) -> Result<Vec<u8>, String> {
    let mut stream = TcpStream::connect(peer_addr)
        .map_err(|e| format!("connect {peer_addr} failed: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(10))).ok();

    // 发请求：[u16 len][hash]
    let hash_bytes = hash.as_bytes();
    let len = hash_bytes.len() as u16;
    stream
        .write_all(&len.to_be_bytes())
        .map_err(|e| format!("send len failed: {e}"))?;
    stream
        .write_all(hash_bytes)
        .map_err(|e| format!("send hash failed: {e}"))?;
    stream.flush().ok();

    // 读响应：[u32 len][bytes]
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .map_err(|e| format!("read len failed: {e}"))?;
    let data_len = u32::from_be_bytes(len_buf) as usize;
    if data_len == 0 {
        return Err(format!("peer {peer_addr} does not have chunk {hash}"));
    }

    let mut data = vec![0u8; data_len];
    stream
        .read_exact(&mut data)
        .map_err(|e| format!("read data failed: {e}"))?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::hash_bytes;

    #[test]
    fn serve_and_fetch_roundtrip() {
        let store = Arc::new(ChunkStore::new());
        let payload = b"a chunk of audio bytes".to_vec();
        let h = hash_bytes(&payload);
        store.put(&h, payload.clone());

        let addr = start_chunk_server(Arc::clone(&store)).unwrap();
        // 从服务拉回来，应与原分片一致。
        let got = fetch_chunk(&addr, &h).unwrap();
        assert_eq!(got, payload);

        // 拉一个不存在的分片，应报错。
        assert!(fetch_chunk(&addr, "deadbeef").is_err());
    }
}

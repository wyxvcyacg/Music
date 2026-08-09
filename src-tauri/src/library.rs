//! Library —— 本地曲库的持久化。
//!
//! 分片持久化解决了"缓存跨重启保留"，但重启后曲目列表仍会清空 —— 用户
//! 看不到自己导入/收藏过什么。这里把曲目清单（manifest + 元信息）存成
//! 一个 JSON 文件，启动时恢复。
//!
//! 曲库还有第二个作用：它定义了"我的收藏"。缓存淘汰时，曲库里曲目用到的
//! 分片受保护、永不删除，其余（流式播放顺带缓存的过路分片）才可淘汰。

use crate::chunk::TrackManifest;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 曲库里的一条曲目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryTrack {
    pub manifest: TrackManifest,
    pub title: String,
    pub artist: String,
    #[serde(default)]
    pub mime: String,
    /// 加入时间（Unix 秒）。仅用于展示/排序，不参与淘汰决策。
    #[serde(default)]
    pub added_at: u64,
}

/// 持久化的本地曲库。
pub struct Library {
    /// JSON 文件路径；None 表示纯内存（测试用）。
    path: Option<PathBuf>,
    tracks: Mutex<Vec<LibraryTrack>>,
}

impl Library {
    /// 纯内存曲库（测试用）。
    pub fn new() -> Self {
        Self {
            path: None,
            tracks: Mutex::new(Vec::new()),
        }
    }

    /// 从 JSON 文件打开曲库。文件不存在视为空库（首次启动）。
    /// 文件损坏时也视为空库并保留原文件备份，不让坏数据阻断启动。
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let tracks = match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Vec<LibraryTrack>>(&text) {
                Ok(list) => list,
                Err(e) => {
                    eprintln!("[library] {} is corrupt ({e}), starting empty", path.display());
                    // 保留一份备份便于排查，不静默丢弃用户数据。
                    let _ = fs::rename(&path, path.with_extension("json.bak"));
                    Vec::new()
                }
            },
            Err(_) => Vec::new(), // 不存在 = 首次启动
        };
        Self {
            path: Some(path),
            tracks: Mutex::new(tracks),
        }
    }

    /// 当前曲库快照。
    pub fn list(&self) -> Vec<LibraryTrack> {
        self.tracks.lock().unwrap().clone()
    }

    /// 加入一首曲目（按 track_hash 去重：已存在则更新元信息）。
    pub fn add(&self, track: LibraryTrack) {
        {
            let mut g = self.tracks.lock().unwrap();
            match g
                .iter_mut()
                .find(|t| t.manifest.track_hash == track.manifest.track_hash)
            {
                Some(existing) => *existing = track,
                None => g.push(track),
            }
        }
        self.save();
    }

    /// 从曲库移除一首曲目。返回是否移除了。
    /// 注意：只移除曲库条目，不删分片 —— 分片由缓存淘汰按需回收。
    pub fn remove(&self, track_hash: &str) -> bool {
        let removed = {
            let mut g = self.tracks.lock().unwrap();
            let before = g.len();
            g.retain(|t| t.manifest.track_hash != track_hash);
            g.len() != before
        };
        if removed {
            self.save();
        }
        removed
    }

    /// 曲库所有曲目用到的分片哈希并集 —— 这些分片受保护、不被淘汰。
    pub fn protected_hashes(&self) -> HashSet<String> {
        let g = self.tracks.lock().unwrap();
        g.iter()
            .flat_map(|t| t.manifest.chunks.iter().cloned())
            .collect()
    }

    /// 写盘。整文件重写 —— 曲目量级小（几百条也就几十 KB），
    /// 换来实现简单和崩溃安全（配合原子 rename）。
    fn save(&self) {
        let Some(path) = &self.path else { return };
        let snapshot = self.tracks.lock().unwrap().clone();
        let json = match serde_json::to_string_pretty(&snapshot) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("[library] serialize failed: {e}");
                return;
            }
        };
        if let Some(parent) = path.parent() {
            if fs::create_dir_all(parent).is_err() {
                return;
            }
        }
        // 原子写：先写 .tmp 再 rename，避免崩溃留下半截 JSON。
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, json).is_err() {
            return;
        }
        if fs::rename(&tmp, path).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }
}

impl Default for Library {
    fn default() -> Self {
        Self::new()
    }
}

/// 当前 Unix 时间戳（秒）。取不到时钟时返回 0。
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 便捷构造：从 manifest + 元信息建一条曲库记录。
pub fn make_track(
    manifest: TrackManifest,
    title: impl Into<String>,
    artist: impl Into<String>,
    mime: impl Into<String>,
) -> LibraryTrack {
    LibraryTrack {
        manifest,
        title: title.into(),
        artist: artist.into(),
        mime: mime.into(),
        added_at: now_secs(),
    }
}

/// 曲库 JSON 的默认文件名（放在 app_data_dir 下）。
pub const LIBRARY_FILE: &str = "library.json";

/// 供 lib.rs 拼路径用。
pub fn library_path(base: &Path) -> PathBuf {
    base.join(LIBRARY_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir()
            .join(format!("music_lib_test_{}_{}", std::process::id(), n))
            .join(format!("{tag}.json"));
        let _ = fs::remove_dir_all(p.parent().unwrap());
        p
    }

    fn manifest(hash: &str, chunks: &[&str]) -> TrackManifest {
        TrackManifest {
            track_hash: hash.into(),
            total_size: 100,
            chunk_size: 256,
            chunks: chunks.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn library_persists_across_reopen() {
        let path = temp_path("persist");

        {
            let lib = Library::open(&path);
            lib.add(make_track(
                manifest("track1", &["c1", "c2"]),
                "Song One",
                "Artist",
                "audio/mpeg",
            ));
            assert_eq!(lib.list().len(), 1);
        } // 模拟退出

        {
            let reopened = Library::open(&path);
            let list = reopened.list();
            assert_eq!(list.len(), 1, "library lost across reopen");
            assert_eq!(list[0].title, "Song One");
            assert_eq!(list[0].manifest.chunks, vec!["c1", "c2"]);
        }

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn add_dedups_by_track_hash() {
        let lib = Library::new();
        lib.add(make_track(manifest("t", &["c1"]), "Old Title", "A", ""));
        lib.add(make_track(manifest("t", &["c1"]), "New Title", "A", ""));
        let list = lib.list();
        assert_eq!(list.len(), 1, "same track_hash should not duplicate");
        assert_eq!(list[0].title, "New Title", "should update in place");
    }

    #[test]
    fn remove_drops_entry() {
        let lib = Library::new();
        lib.add(make_track(manifest("t1", &["c1"]), "One", "A", ""));
        lib.add(make_track(manifest("t2", &["c2"]), "Two", "A", ""));

        assert!(lib.remove("t1"));
        assert_eq!(lib.list().len(), 1);
        // 再删同一个应返回 false
        assert!(!lib.remove("t1"));
    }

    #[test]
    fn protected_hashes_unions_all_chunks() {
        let lib = Library::new();
        lib.add(make_track(manifest("t1", &["a", "b"]), "One", "A", ""));
        lib.add(make_track(manifest("t2", &["b", "c"]), "Two", "A", ""));

        let protected = lib.protected_hashes();
        // b 出现在两首里，应去重
        assert_eq!(protected.len(), 3);
        for h in ["a", "b", "c"] {
            assert!(protected.contains(h), "missing {h}");
        }
    }

    #[test]
    fn corrupt_file_starts_empty_and_backs_up() {
        let path = temp_path("corrupt");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{ this is not valid json").unwrap();

        let lib = Library::open(&path);
        assert!(lib.list().is_empty(), "corrupt file should yield empty lib");
        // 原文件被挪成 .bak，便于排查而非静默丢弃。
        assert!(
            path.with_extension("json.bak").exists(),
            "corrupt file should be backed up"
        );

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}

//! Playlist —— 播放列表的持久化。
//!
//! 与曲库的分工：曲库是"我有什么"（一首歌一条记录，含完整 manifest），
//! 播放列表是"我想按什么顺序听"（只存 `track_hash` 引用 + 顺序）。
//!
//! **只存引用不存副本**是刻意的：
//! - 同一首歌进 10 个列表，不会有 10 份 manifest 副本要同步；
//! - 在曲库里改了标题/艺术家，所有列表立刻一致；
//! - 一首歌从曲库移除后，列表里那条会解析不到 —— 前端标成"不可用"，
//!   而不是留一条能点但播不出来的死记录。
//!
//! 顺序用 `Vec` 而不是 `HashSet`：播放列表的顺序本身就是用户数据。
//! 同一首歌允许在一个列表里出现多次（用户可能真想连播两遍）。

use crate::library::now_secs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 一个播放列表。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Playlist {
    /// 稳定标识。重命名不影响引用。
    pub id: String,
    pub name: String,
    /// 曲目引用，**顺序即播放顺序**。
    #[serde(default)]
    pub tracks: Vec<String>,
    #[serde(default)]
    pub created_at: u64,
}

/// 持久化的播放列表集合。
pub struct Playlists {
    /// JSON 文件路径；None 表示纯内存（测试用）。
    path: Option<PathBuf>,
    lists: Mutex<Vec<Playlist>>,
}

impl Playlists {
    /// 纯内存（测试用）。
    pub fn new() -> Self {
        Self {
            path: None,
            lists: Mutex::new(Vec::new()),
        }
    }

    /// 从 JSON 文件打开。文件不存在视为空（首次启动）；
    /// 损坏时备份原文件并从空开始，不让坏数据阻断启动。
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let lists = match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Vec<Playlist>>(&text) {
                Ok(list) => list,
                Err(e) => {
                    eprintln!(
                        "[playlist] {} is corrupt ({e}), starting empty",
                        path.display()
                    );
                    let _ = fs::rename(&path, path.with_extension("json.bak"));
                    Vec::new()
                }
            },
            Err(_) => Vec::new(),
        };
        Self {
            path: Some(path),
            lists: Mutex::new(lists),
        }
    }

    /// 当前所有播放列表快照。
    pub fn list(&self) -> Vec<Playlist> {
        self.lists.lock().unwrap().clone()
    }

    /// 新建一个空列表，返回它的 id。
    pub fn create(&self, name: impl Into<String>) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        {
            let mut g = self.lists.lock().unwrap();
            g.push(Playlist {
                id: id.clone(),
                name: name.into(),
                tracks: Vec::new(),
                created_at: now_secs(),
            });
        }
        self.save();
        id
    }

    /// 重命名。列表不存在时返回 false。
    pub fn rename(&self, id: &str, name: impl Into<String>) -> bool {
        let ok = {
            let mut g = self.lists.lock().unwrap();
            match g.iter_mut().find(|p| p.id == id) {
                Some(p) => {
                    p.name = name.into();
                    true
                }
                None => false,
            }
        };
        if ok {
            self.save();
        }
        ok
    }

    /// 删除整个列表。曲目本身与分片都不受影响。
    pub fn delete(&self, id: &str) -> bool {
        let removed = {
            let mut g = self.lists.lock().unwrap();
            let before = g.len();
            g.retain(|p| p.id != id);
            g.len() != before
        };
        if removed {
            self.save();
        }
        removed
    }

    /// 往列表末尾追加一首。列表不存在时返回 false。
    pub fn add_track(&self, id: &str, track_hash: impl Into<String>) -> bool {
        let ok = {
            let mut g = self.lists.lock().unwrap();
            match g.iter_mut().find(|p| p.id == id) {
                Some(p) => {
                    p.tracks.push(track_hash.into());
                    true
                }
                None => false,
            }
        };
        if ok {
            self.save();
        }
        ok
    }

    /// 按**位置**移除一首。
    ///
    /// 用位置而不是 track_hash：同一首歌允许重复出现，按 hash 删会
    /// 一次干掉全部，那不是用户点第 3 行时想要的结果。
    pub fn remove_at(&self, id: &str, index: usize) -> bool {
        let ok = {
            let mut g = self.lists.lock().unwrap();
            match g.iter_mut().find(|p| p.id == id) {
                Some(p) if index < p.tracks.len() => {
                    p.tracks.remove(index);
                    true
                }
                _ => false,
            }
        };
        if ok {
            self.save();
        }
        ok
    }

    /// 整表替换曲目顺序（前端拖拽排序后一次提交）。
    pub fn reorder(&self, id: &str, tracks: Vec<String>) -> bool {
        let ok = {
            let mut g = self.lists.lock().unwrap();
            match g.iter_mut().find(|p| p.id == id) {
                Some(p) => {
                    p.tracks = tracks;
                    true
                }
                None => false,
            }
        };
        if ok {
            self.save();
        }
        ok
    }

    /// 写盘。整文件重写 + 原子 rename，与曲库一致。
    fn save(&self) {
        let Some(path) = &self.path else { return };
        let snapshot = self.lists.lock().unwrap().clone();
        let json = match serde_json::to_string_pretty(&snapshot) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("[playlist] serialize failed: {e}");
                return;
            }
        };
        if let Some(parent) = path.parent() {
            if fs::create_dir_all(parent).is_err() {
                return;
            }
        }
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, json).is_err() {
            return;
        }
        if fs::rename(&tmp, path).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }
}

impl Default for Playlists {
    fn default() -> Self {
        Self::new()
    }
}

/// 播放列表 JSON 的默认文件名（放在 app_data_dir 下）。
pub const PLAYLIST_FILE: &str = "playlists.json";

/// 供 lib.rs 拼路径用。
pub fn playlists_path(base: &Path) -> PathBuf {
    base.join(PLAYLIST_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir()
            .join(format!("music_pl_test_{}_{}", std::process::id(), n))
            .join(format!("{tag}.json"));
        let _ = fs::remove_dir_all(p.parent().unwrap());
        p
    }

    #[test]
    fn create_and_add_tracks() {
        let pls = Playlists::new();
        let id = pls.create("我的最爱");
        assert!(pls.add_track(&id, "hash1"));
        assert!(pls.add_track(&id, "hash2"));

        let all = pls.list();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "我的最爱");
        assert_eq!(all[0].tracks, vec!["hash1", "hash2"]);
    }

    #[test]
    fn order_is_preserved_and_duplicates_allowed() {
        let pls = Playlists::new();
        let id = pls.create("p");
        for h in ["c", "a", "c", "b"] {
            pls.add_track(&id, h);
        }
        // 顺序是用户数据：既不排序也不去重。
        assert_eq!(pls.list()[0].tracks, vec!["c", "a", "c", "b"]);
    }

    #[test]
    fn remove_at_targets_position_not_all_matches() {
        let pls = Playlists::new();
        let id = pls.create("p");
        for h in ["a", "dup", "b", "dup"] {
            pls.add_track(&id, h);
        }
        // 删第 1 位（第一个 dup），另一个 dup 必须留下。
        assert!(pls.remove_at(&id, 1));
        assert_eq!(pls.list()[0].tracks, vec!["a", "b", "dup"]);
    }

    #[test]
    fn remove_at_out_of_range_is_rejected() {
        let pls = Playlists::new();
        let id = pls.create("p");
        pls.add_track(&id, "a");
        assert!(!pls.remove_at(&id, 5), "越界删除应返回 false 而非 panic");
        assert_eq!(pls.list()[0].tracks.len(), 1);
    }

    #[test]
    fn rename_and_delete() {
        let pls = Playlists::new();
        let id = pls.create("旧名");
        assert!(pls.rename(&id, "新名"));
        assert_eq!(pls.list()[0].name, "新名");

        assert!(pls.delete(&id));
        assert!(pls.list().is_empty());
        // 再删同一个应返回 false
        assert!(!pls.delete(&id));
    }

    #[test]
    fn operations_on_missing_playlist_return_false() {
        let pls = Playlists::new();
        assert!(!pls.add_track("nope", "h"));
        assert!(!pls.rename("nope", "x"));
        assert!(!pls.remove_at("nope", 0));
        assert!(!pls.reorder("nope", vec![]));
    }

    #[test]
    fn reorder_replaces_sequence() {
        let pls = Playlists::new();
        let id = pls.create("p");
        for h in ["a", "b", "c"] {
            pls.add_track(&id, h);
        }
        assert!(pls.reorder(&id, vec!["c".into(), "a".into(), "b".into()]));
        assert_eq!(pls.list()[0].tracks, vec!["c", "a", "b"]);
    }

    #[test]
    fn playlists_persist_across_reopen() {
        let path = temp_path("persist");
        let id = {
            let pls = Playlists::open(&path);
            let id = pls.create("跨重启");
            pls.add_track(&id, "h1");
            pls.add_track(&id, "h2");
            id
        }; // 模拟退出

        {
            let reopened = Playlists::open(&path);
            let all = reopened.list();
            assert_eq!(all.len(), 1, "playlists lost across reopen");
            assert_eq!(all[0].id, id, "id must be stable across reopen");
            assert_eq!(all[0].name, "跨重启");
            assert_eq!(all[0].tracks, vec!["h1", "h2"]);
        }

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn corrupt_file_starts_empty_and_backs_up() {
        let path = temp_path("corrupt");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"not json at all").unwrap();

        let pls = Playlists::open(&path);
        assert!(pls.list().is_empty());
        assert!(
            path.with_extension("json.bak").exists(),
            "corrupt file should be backed up, not silently dropped"
        );

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}

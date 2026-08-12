import { invoke } from "@tauri-apps/api/core";

/** 与 Rust chunk.rs 的 TrackManifest 对应。 */
export type TrackManifest = {
  track_hash: string;
  total_size: number;
  chunk_size: number;
  chunks: string[];
};

/** 与 Rust lib.rs 的 P2pStatus 对应。 */
export type P2pStatus = {
  peer_id: string;
  chunk_addr: string;
  /** 本节点 UDP 端口；null = UDP 不可用，只走 TCP。 */
  udp_addr: string | null;
  /** Tracker 观测到的本机地址。与 chunk_addr 的 IP 不同即说明经过了 NAT。 */
  observed_addr: string | null;
  tracker_online: boolean;
  owned_chunks: number;
};

/** 与 Rust tracker.rs 的 SharedTrack 对应 —— 共享曲库里的一条曲目。 */
export type SharedTrack = {
  manifest: TrackManifest;
  title: string;
  artist: string;
  mime: string;
  /** 发布者用户名（阶段三）。 */
  publisher: string;
};

// ---- 账号（阶段三）----
// 注意：节点身份（peer_id）与用户身份（username）是解耦的两层。
// 登出不影响 P2P 传输 —— 分片照常供源。

/** 当前登录用户名；未登录返回 null。 */
export async function currentUser(): Promise<string | null> {
  return invoke<string | null>("current_user");
}

/** 注册新账号（成功即登录），返回用户名。 */
export async function registerAccount(
  username: string,
  password: string
): Promise<string> {
  return invoke<string>("register_account", { username, password });
}

/** 登录，返回用户名。 */
export async function login(username: string, password: string): Promise<string> {
  return invoke<string>("login", { username, password });
}

/** 登出（不影响 P2P 传输）。 */
export async function logout(): Promise<void> {
  return invoke("logout");
}

/** 与 Rust lib.rs 的 DownloadResult 对应。 */
export type DownloadResult = {
  data: Uint8Array;
  fetched: number;
  cached: number;
};

/** 是否运行在 Tauri 环境内（浏览器 dev 时为 false）。 */
export function isTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/**
 * 导入一首曲目：把字节送进 Rust，切片 + 算哈希 + 存入 ChunkStore，
 * 向 Tracker 宣告，并加入持久化曲库。
 */
export async function importTrack(
  data: Uint8Array,
  title: string,
  artist: string,
  mime: string
): Promise<TrackManifest> {
  // Tauri 的 invoke 会把数组序列化为 JSON number[]；
  // 对音频这类大数据这是阶段一/二的可接受折中，后续改用流式/二进制通道。
  return invoke<TrackManifest>("import_track", {
    data: Array.from(data),
    title,
    artist,
    mime,
  });
}

/** 与 Rust library.rs 的 LibraryTrack 对应 —— 持久化曲库里的一条曲目。 */
export type LibraryTrack = {
  manifest: TrackManifest;
  title: string;
  artist: string;
  mime: string;
  added_at: number;
};

/** 列出持久化的本地曲库（启动时恢复）。 */
export async function listLibrary(): Promise<LibraryTrack[]> {
  return invoke<LibraryTrack[]>("list_library");
}

/** 从曲库移除一首曲目（分片留给缓存淘汰回收）。 */
export async function removeFromLibrary(trackHash: string): Promise<boolean> {
  return invoke<boolean>("remove_from_library", { trackHash });
}

/** 与 Rust playlist.rs 的 Playlist 对应。`tracks` 是 track_hash 引用，顺序即播放顺序。 */
export type Playlist = {
  id: string;
  name: string;
  tracks: string[];
  created_at: number;
};

/** 列出所有播放列表。 */
export async function listPlaylists(): Promise<Playlist[]> {
  return invoke<Playlist[]>("list_playlists");
}

/** 新建播放列表，返回它的 id。 */
export async function createPlaylist(name: string): Promise<string> {
  return invoke<string>("create_playlist", { name });
}

/** 重命名播放列表。 */
export async function renamePlaylist(
  id: string,
  name: string
): Promise<boolean> {
  return invoke<boolean>("rename_playlist", { id, name });
}

/** 删除播放列表。不影响曲库与分片。 */
export async function deletePlaylist(id: string): Promise<boolean> {
  return invoke<boolean>("delete_playlist", { id });
}

/** 往播放列表末尾追加一首曲目。 */
export async function addToPlaylist(
  id: string,
  trackHash: string
): Promise<boolean> {
  return invoke<boolean>("add_to_playlist", { id, trackHash });
}

/** 按位置从播放列表移除一首（允许重复曲目，所以用位置而非 hash）。 */
export async function removeFromPlaylist(
  id: string,
  index: number
): Promise<boolean> {
  return invoke<boolean>("remove_from_playlist", { id, index });
}

/** 整表提交新的曲目顺序。 */
export async function reorderPlaylist(
  id: string,
  tracks: string[]
): Promise<boolean> {
  return invoke<boolean>("reorder_playlist", { id, tracks });
}

/** 发布一首曲目到共享曲库（Tracker 上的其他节点可发现并下载）。 */
export async function publishTrack(
  manifest: TrackManifest,
  title: string,
  artist: string,
  mime: string
): Promise<void> {
  return invoke("publish_track", { manifest, title, artist, mime });
}

/**
 * 登记一首曲目为可流式播放，返回 `<audio>` 可直接用的 stream URL。
 * 分片可尚未全部在本地 —— 播放时由 stream:// 协议按需从 peer 拉取（边下边播）。
 */
export async function prepareStream(
  manifest: TrackManifest,
  mime: string
): Promise<string> {
  return invoke<string>("prepare_stream", { manifest, mime });
}

/** 列出 Tracker 上所有已发布的共享曲目。 */
export async function listShared(): Promise<SharedTrack[]> {
  return invoke<SharedTrack[]>("list_shared");
}

/** 按 track_hash 查询单个共享曲目。返回 null 表示 Tracker 上没有。 */
export async function lookupTrack(trackHash: string): Promise<SharedTrack | null> {
  return invoke<SharedTrack | null>("lookup_track", { trackHash });
}

/**
 * 曲目资源标识符 —— `music://track/<sha256>`。
 * 这就是架构铁律之三"内容哈希寻址"对用户可见的形态：
 * 拿到这串就能从 Tracker 找曲目并流式播放。
 */
export const TRACK_URI_SCHEME = "music://track/";

export function makeTrackUri(trackHash: string): string {
  return `${TRACK_URI_SCHEME}${trackHash}`;
}

/** 从粘贴板内容里解析 track hash；接受完整 URI 或裸 hash。 */
export function parseTrackInput(input: string): string | null {
  const s = input.trim();
  if (!s) return null;
  const m = s.match(/^music:\/\/track\/([a-fA-F0-9]{64})$/);
  if (m) return m[1].toLowerCase();
  if (/^[a-fA-F0-9]{64}$/.test(s)) return s.toLowerCase();
  return null;
}

/**
 * 按清单下载一首曲目：Rust 逐分片从其他节点拉取、校验入库、重组，
 * 返回完整字节 + 本次下载/命中缓存的分片数。
 */
export async function downloadTrack(
  manifest: TrackManifest,
  title: string,
  artist: string,
  mime: string
): Promise<DownloadResult> {
  const r = await invoke<{ data: number[]; fetched: number; cached: number }>(
    "download_track",
    { manifest, title, artist, mime }
  );
  return { data: new Uint8Array(r.data), fetched: r.fetched, cached: r.cached };
}

/** 按清单从 ChunkStore 重组出完整曲目字节。缺片则 reject。 */
export async function reassemble(manifest: TrackManifest): Promise<Uint8Array> {
  const bytes = await invoke<number[]>("reassemble", { manifest });
  return new Uint8Array(bytes);
}

/** 某分片本地是否持有。 */
export async function hasChunk(chunkHash: string): Promise<boolean> {
  return invoke<boolean>("has_chunk", { chunkHash });
}

/** P2P 运行状态快照。 */
export async function p2pStatus(): Promise<P2pStatus> {
  return invoke<P2pStatus>("p2p_status");
}

/** 与 Rust lib.rs 的 CacheStats 对应 —— 本地分片缓存统计。 */
export type CacheStats = {
  chunks: number;
  bytes: number;
  limit: number;
};

/** 本地分片缓存统计（磁盘占用）。 */
export async function cacheStats(): Promise<CacheStats> {
  return invoke<CacheStats>("cache_stats");
}

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
  tracker_online: boolean;
  owned_chunks: number;
};

/** 与 Rust tracker.rs 的 SharedTrack 对应 —— 共享曲库里的一条曲目。 */
export type SharedTrack = {
  manifest: TrackManifest;
  title: string;
  artist: string;
  mime: string;
};

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
 * 并向 Tracker 宣告，返回清单。
 */
export async function importTrack(data: Uint8Array): Promise<TrackManifest> {
  // Tauri 的 invoke 会把数组序列化为 JSON number[]；
  // 对音频这类大数据这是阶段一/二的可接受折中，后续改用流式/二进制通道。
  return invoke<TrackManifest>("import_track", { data: Array.from(data) });
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
export async function downloadTrack(manifest: TrackManifest): Promise<DownloadResult> {
  const r = await invoke<{ data: number[]; fetched: number; cached: number }>(
    "download_track",
    { manifest }
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

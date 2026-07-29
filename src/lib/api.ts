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
  peer_count: number;
  owned_chunks: number;
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
  // 对音频这类大数据这是阶段一的可接受折中，阶段二改用流式/二进制通道。
  return invoke<TrackManifest>("import_track", { data: Array.from(data) });
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

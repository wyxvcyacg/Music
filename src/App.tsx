import { useEffect, useRef, useState } from "react";
import {
  Play,
  Pause,
  SkipBack,
  SkipForward,
  Shuffle,
  Repeat,
  Volume2,
  Library,
  Search,
  ListMusic,
  Share2,
  Users,
  Plus,
  Boxes,
  Loader2,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { usePlayer, fmtTime } from "@/hooks/usePlayer";
import {
  importTrack,
  reassemble,
  p2pStatus,
  isTauri,
  type TrackManifest,
  type P2pStatus,
} from "@/lib/api";

type Track = {
  id: number;
  title: string;
  artist: string;
  /** 可播放音源 URL（Blob / object URL）。示例曲目为 undefined。 */
  url?: string;
  duration: string;
  peers: number;
  /** 经 Rust 切片后的清单（内容哈希 + 分片列表）。 */
  manifest?: TrackManifest;
  /** 导入处理状态。 */
  status?: "importing" | "ready" | "error";
};

const DEMO_TRACKS: Track[] = [
  { id: 1, title: "Midnight City Lights", artist: "Neon Drive", duration: "3:42", peers: 12 },
  { id: 2, title: "Echoes in the Rain", artist: "Lo-Fi Collective", duration: "4:15", peers: 8 },
  { id: 3, title: "Digital Horizon", artist: "Synthwave Kid", duration: "5:01", peers: 24 },
  { id: 4, title: "Ocean of Static", artist: "Ambient Waves", duration: "6:20", peers: 3 },
];

function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

function App() {
  const player = usePlayer();
  const [tracks, setTracks] = useState<Track[]>(DEMO_TRACKS);
  const [current, setCurrent] = useState<Track | null>(null);
  const [status, setStatus] = useState<P2pStatus | null>(null);
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const nextId = useRef(100);

  // 轮询 P2P 状态（在 Tauri 内才有后端）。
  useEffect(() => {
    if (!isTauri()) return;
    let alive = true;
    const tick = () => p2pStatus().then((s) => alive && setStatus(s)).catch(() => {});
    tick();
    const timer = setInterval(tick, 2000);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, []);

  function playTrack(t: Track) {
    setCurrent(t);
    if (t.url) player.load(t.url, true);
  }

  function patchTrack(id: number, patch: Partial<Track>) {
    setTracks((prev) => prev.map((t) => (t.id === id ? { ...t, ...patch } : t)));
    setCurrent((c) => (c && c.id === id ? { ...c, ...patch } : c));
  }

  /**
   * 导入一个文件。在 Tauri 内走完整 P2P 路径：
   *   bytes -> import_track(切片+哈希+入库) -> reassemble(重组) -> 播放重组结果
   * 浏览器 dev 环境下降级为普通 object URL。
   */
  async function importOne(file: File): Promise<Track> {
    const id = nextId.current++;
    const base: Track = {
      id,
      title: file.name.replace(/\.[^.]+$/, ""),
      artist: "本地导入",
      duration: "--:--",
      peers: 1,
      status: "importing",
    };

    // 先把占位行放进列表。
    setTracks((prev) => [base, ...prev]);

    if (!isTauri()) {
      // 浏览器环境：无后端，直接用 object URL。
      const url = URL.createObjectURL(file);
      const ready = { ...base, url, status: "ready" as const };
      patchTrack(id, ready);
      return ready;
    }

    try {
      const bytes = new Uint8Array(await file.arrayBuffer());
      // 1. 送进 Rust：切片 + SHA-256 + 存入 ChunkStore + 向 Tracker 宣告
      const manifest = await importTrack(bytes);
      // 2. 从 ChunkStore 按清单重组
      const restored = await reassemble(manifest);
      // 3. 无损校验：字节数须与原文件一致
      if (restored.length !== bytes.length) {
        throw new Error(`length mismatch: ${restored.length} != ${bytes.length}`);
      }
      // 4. 用重组后的字节构造可播放的 Blob URL
      const blob = new Blob([restored], { type: file.type || "audio/mpeg" });
      const url = URL.createObjectURL(blob);
      const ready = { ...base, url, manifest, status: "ready" as const };
      patchTrack(id, ready);
      return ready;
    } catch (e) {
      console.error("import failed", e);
      patchTrack(id, { status: "error" });
      return { ...base, status: "error" };
    }
  }

  async function onImportFiles(files: FileList | null) {
    if (!files || files.length === 0) return;
    const list = Array.from(files);
    // 逐个导入，第一首就绪后自动播放。
    let first = true;
    for (const f of list) {
      const t = await importOne(f);
      if (first && t.url) {
        playTrack(t);
        first = false;
      }
    }
  }

  const played = player.duration ? (player.currentTime / player.duration) * 100 : 0;
  const buffered = player.duration ? (player.buffered / player.duration) * 100 : 0;

  return (
    <div className="dark flex h-screen flex-col bg-background text-foreground">
      <input
        ref={fileInputRef}
        type="file"
        accept="audio/*"
        multiple
        hidden
        onChange={(e) => onImportFiles(e.target.files)}
      />

      <div className="flex flex-1 overflow-hidden">
        {/* sidebar */}
        <aside className="flex w-60 flex-col gap-1 border-r border-border bg-card/40 p-3">
          <div className="mb-4 flex items-center gap-2 px-2">
            <div className="flex size-8 items-center justify-center rounded-md bg-primary text-primary-foreground">
              <ListMusic className="size-5" />
            </div>
            <span className="text-lg font-semibold">Music</span>
          </div>
          <NavItem icon={<Search className="size-4" />} label="搜索" />
          <NavItem icon={<Library className="size-4" />} label="曲库" active />
          <NavItem icon={<ListMusic className="size-4" />} label="播放列表" />
          <NavItem icon={<Users className="size-4" />} label="节点网络" />

          <div className="mt-auto space-y-2 rounded-md bg-secondary/50 p-3 text-xs text-muted-foreground">
            <div className="flex items-center gap-1.5 font-medium text-foreground">
              <Share2 className="size-3.5" /> P2P 状态
            </div>
            {status ? (
              <>
                <StatusRow label="在线节点" value={`${status.peer_count}`} />
                <StatusRow label="本地分片" value={`${status.owned_chunks}`} />
                <div className="truncate pt-1 text-[10px] text-muted-foreground/70">
                  peer: {status.peer_id.slice(0, 8)}…
                </div>
              </>
            ) : (
              <div className="text-[11px] text-muted-foreground/70">
                {isTauri() ? "连接中…" : "浏览器预览（无后端）"}
              </div>
            )}
          </div>
        </aside>

        {/* track list */}
        <main className="flex-1 overflow-y-auto p-6">
          <div className="mb-6 flex items-start justify-between">
            <div>
              <h1 className="text-2xl font-bold">曲库</h1>
              <p className="text-sm text-muted-foreground">
                边下边播 · 多源加速 · 人越多播放越流畅
              </p>
            </div>
            <Button onClick={() => fileInputRef.current?.click()}>
              <Plus /> 导入音乐
            </Button>
          </div>

          <div className="overflow-hidden rounded-lg border border-border">
            <table className="w-full text-sm">
              <thead className="bg-secondary/40 text-left text-xs text-muted-foreground">
                <tr>
                  <th className="w-10 px-4 py-2.5">#</th>
                  <th className="px-4 py-2.5">标题</th>
                  <th className="px-4 py-2.5">艺术家</th>
                  <th className="px-4 py-2.5">
                    <span className="flex items-center gap-1">
                      <Boxes className="size-3.5" /> 分片
                    </span>
                  </th>
                  <th className="px-4 py-2.5">
                    <span className="flex items-center gap-1">
                      <Users className="size-3.5" /> 节点
                    </span>
                  </th>
                  <th className="px-4 py-2.5 text-right">时长</th>
                </tr>
              </thead>
              <tbody>
                {tracks.map((t, i) => (
                  <tr
                    key={t.id}
                    onClick={() => t.url && playTrack(t)}
                    className={cn(
                      "border-t border-border transition-colors",
                      t.url
                        ? "cursor-pointer hover:bg-accent/50"
                        : "cursor-default",
                      current?.id === t.id && "bg-accent/40",
                      !t.url && "opacity-60"
                    )}
                    title={
                      t.manifest
                        ? `track ${t.manifest.track_hash.slice(0, 12)}… · ${fmtBytes(
                            t.manifest.total_size
                          )}`
                        : t.url
                          ? ""
                          : "示例曲目（无音源）—— 点「导入音乐」添加可播放文件"
                    }
                  >
                    <td className="px-4 py-3 text-muted-foreground">{i + 1}</td>
                    <td className="px-4 py-3 font-medium">
                      <span className="flex items-center gap-2">
                        {t.status === "importing" && (
                          <Loader2 className="size-3.5 animate-spin text-primary" />
                        )}
                        {t.title}
                        {t.status === "error" && (
                          <span className="text-xs text-destructive">导入失败</span>
                        )}
                      </span>
                    </td>
                    <td className="px-4 py-3 text-muted-foreground">{t.artist}</td>
                    <td className="px-4 py-3 text-muted-foreground">
                      {t.manifest ? (
                        <span className="tabular-nums">{t.manifest.chunks.length}</span>
                      ) : (
                        "—"
                      )}
                    </td>
                    <td className="px-4 py-3">
                      <span className="inline-flex items-center gap-1 rounded-full bg-primary/10 px-2 py-0.5 text-xs text-primary">
                        {t.peers}
                      </span>
                    </td>
                    <td className="px-4 py-3 text-right text-muted-foreground">
                      {t.duration}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </main>
      </div>

      {/* playback bar */}
      <footer className="border-t border-border bg-card/60 px-4 py-3">
        <div className="flex items-center gap-4">
          <div className="flex w-60 items-center gap-3">
            <div className="flex size-12 shrink-0 items-center justify-center rounded-md bg-gradient-to-br from-primary/30 to-primary/5">
              <ListMusic className="size-5 text-primary" />
            </div>
            <div className="min-w-0">
              <div className="truncate text-sm font-medium">
                {current?.title ?? "未在播放"}
              </div>
              <div className="truncate text-xs text-muted-foreground">
                {current?.artist ?? "点一首曲目开始"}
              </div>
            </div>
          </div>

          <div className="flex flex-1 flex-col items-center gap-1.5">
            <div className="flex items-center gap-2">
              <Button variant="ghost" size="icon" className="text-muted-foreground">
                <Shuffle />
              </Button>
              <Button variant="ghost" size="icon">
                <SkipBack />
              </Button>
              <Button
                size="icon"
                className="size-10 rounded-full"
                onClick={() => player.toggle()}
              >
                {player.playing ? <Pause /> : <Play />}
              </Button>
              <Button variant="ghost" size="icon">
                <SkipForward />
              </Button>
              <Button variant="ghost" size="icon" className="text-muted-foreground">
                <Repeat />
              </Button>
            </div>
            <div className="flex w-full max-w-xl items-center gap-2 text-xs text-muted-foreground">
              <span className="w-9 text-right tabular-nums">
                {fmtTime(player.currentTime)}
              </span>
              <div
                className="group relative h-1.5 flex-1 cursor-pointer overflow-hidden rounded-full bg-secondary"
                onClick={(e) => {
                  if (!player.duration) return;
                  const rect = e.currentTarget.getBoundingClientRect();
                  const ratio = (e.clientX - rect.left) / rect.width;
                  player.seek(ratio * player.duration);
                }}
              >
                <div
                  className="absolute inset-y-0 left-0 bg-muted-foreground/40"
                  style={{ width: `${buffered}%` }}
                />
                <div
                  className="absolute inset-y-0 left-0 bg-primary"
                  style={{ width: `${played}%` }}
                />
              </div>
              <span className="w-9 tabular-nums">{fmtTime(player.duration)}</span>
            </div>
          </div>

          <div className="flex w-60 items-center justify-end gap-2">
            <Volume2 className="size-4 text-muted-foreground" />
            <input
              type="range"
              min={0}
              max={1}
              step={0.01}
              value={player.volume}
              onChange={(e) => player.setVolume(parseFloat(e.target.value))}
              className="h-1 w-24 cursor-pointer accent-primary"
            />
          </div>
        </div>
      </footer>
    </div>
  );
}

function StatusRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center justify-between">
      <span>{label}</span>
      <span className="font-medium text-foreground tabular-nums">{value}</span>
    </div>
  );
}

function NavItem({
  icon,
  label,
  active,
}: {
  icon: React.ReactNode;
  label: string;
  active?: boolean;
}) {
  return (
    <button
      className={cn(
        "flex items-center gap-3 rounded-md px-3 py-2 text-sm font-medium transition-colors",
        active
          ? "bg-secondary text-foreground"
          : "text-muted-foreground hover:bg-secondary/50 hover:text-foreground"
      )}
    >
      {icon}
      {label}
    </button>
  );
}

export default App;

import { useCallback, useEffect, useRef, useState } from "react";
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
  Upload,
  Download,
  RefreshCw,
  CheckCircle2,
  CircleDot,
  Link as LinkIcon,
  ClipboardPaste,
  Trash2,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { usePlayer, fmtTime } from "@/hooks/usePlayer";
import {
  importTrack,
  reassemble,
  publishTrack,
  prepareStream,
  listShared,
  lookupTrack,
  downloadTrack,
  p2pStatus,
  cacheStats,
  listLibrary,
  removeFromLibrary,
  isTauri,
  makeTrackUri,
  parseTrackInput,
  type TrackManifest,
  type P2pStatus,
  type SharedTrack,
  type CacheStats,
} from "@/lib/api";

type Track = {
  id: number;
  title: string;
  artist: string;
  /** 可播放音源 URL（Blob / object URL / stream URL）。示例曲目为 undefined。 */
  url?: string;
  duration: string;
  manifest?: TrackManifest;
  /** 媒体 MIME 类型（导入时捕获）。 */
  mime?: string;
  status?: "importing" | "ready" | "error";
  /** 是否已发布到共享曲库。 */
  published?: boolean;
};

/// 浏览器预览（无后端）时展示的示例数据；Tauri 内会被真实曲库替换。
const DEMO_TRACKS: Track[] = [
  { id: 1, title: "Midnight City Lights", artist: "Neon Drive", duration: "3:42" },
  { id: 2, title: "Echoes in the Rain", artist: "Lo-Fi Collective", duration: "4:15" },
  { id: 3, title: "Digital Horizon", artist: "Synthwave Kid", duration: "5:01" },
];

function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(1)} GB`;
}

type View = "library" | "network";

function App() {
  const player = usePlayer();
  const [tracks, setTracks] = useState<Track[]>(isTauri() ? [] : DEMO_TRACKS);
  const [current, setCurrent] = useState<Track | null>(null);
  const [status, setStatus] = useState<P2pStatus | null>(null);
  const [cache, setCache] = useState<CacheStats | null>(null);
  const [shared, setShared] = useState<SharedTrack[]>([]);
  const [view, setView] = useState<View>("library");
  /** 正在下载的 track_hash 集合。 */
  const [downloading, setDownloading] = useState<Set<string>>(new Set());
  /** 粘贴链接输入框内容。 */
  const [pasteInput, setPasteInput] = useState("");
  /** 粘贴链接错误提示。 */
  const [pasteError, setPasteError] = useState<string | null>(null);
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const nextId = useRef(100);

  // 轮询 P2P 状态与本地缓存占用。
  useEffect(() => {
    if (!isTauri()) return;
    let alive = true;
    const tick = () => {
      p2pStatus().then((s) => alive && setStatus(s)).catch(() => {});
      cacheStats().then((c) => alive && setCache(c)).catch(() => {});
    };
    tick();
    const timer = setInterval(tick, 2000);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, []);

  // 启动时恢复持久化曲库。分片都在本地，所以 stream URL 可离线秒播。
  useEffect(() => {
    if (!isTauri()) return;
    let alive = true;
    (async () => {
      try {
        const saved = await listLibrary();
        if (!alive || saved.length === 0) return;
        const restored: Track[] = [];
        for (const t of saved) {
          const mime = t.mime || "audio/mpeg";
          let url: string | undefined;
          try {
            url = await prepareStream(t.manifest, mime);
          } catch (e) {
            console.error("prepare stream failed for", t.title, e);
          }
          restored.push({
            id: nextId.current++,
            title: t.title,
            artist: t.artist,
            url,
            manifest: t.manifest,
            mime,
            duration: "--:--",
            status: "ready",
          });
        }
        if (alive) setTracks(restored);
      } catch (e) {
        console.error("restore library failed", e);
      }
    })();
    return () => {
      alive = false;
    };
  }, []);

  const refreshShared = useCallback(async () => {
    if (!isTauri()) return;
    try {
      setShared(await listShared());
    } catch (e) {
      console.error("list shared failed", e);
    }
  }, []);

  // 进入"节点网络"视图时刷新共享曲库。
  useEffect(() => {
    if (view === "network") refreshShared();
  }, [view, refreshShared]);

  function playTrack(t: Track) {
    setCurrent(t);
    if (t.url) player.load(t.url, true);
  }

  function patchTrack(id: number, patch: Partial<Track>) {
    setTracks((prev) => prev.map((t) => (t.id === id ? { ...t, ...patch } : t)));
    setCurrent((c) => (c && c.id === id ? { ...c, ...patch } : c));
  }

  async function importOne(file: File): Promise<Track> {
    const id = nextId.current++;
    const mime = file.type || "audio/mpeg";
    const base: Track = {
      id,
      title: file.name.replace(/\.[^.]+$/, ""),
      artist: "本地导入",
      duration: "--:--",
      mime,
      status: "importing",
    };
    setTracks((prev) => [base, ...prev]);

    if (!isTauri()) {
      const url = URL.createObjectURL(file);
      const ready = { ...base, url, status: "ready" as const };
      patchTrack(id, ready);
      return ready;
    }

    try {
      const bytes = new Uint8Array(await file.arrayBuffer());
      const manifest = await importTrack(bytes, base.title, base.artist, mime);
      const restored = await reassemble(manifest);
      if (restored.length !== bytes.length) {
        throw new Error(`length mismatch: ${restored.length} != ${bytes.length}`);
      }
      const blob = new Blob([restored], { type: mime });
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
    let first = true;
    for (const f of Array.from(files)) {
      const t = await importOne(f);
      if (first && t.url) {
        playTrack(t);
        first = false;
      }
    }
  }

  /** 发布一首已导入的曲目到共享曲库。 */
  async function onPublish(t: Track) {
    if (!t.manifest || !isTauri()) return;
    try {
      await publishTrack(t.manifest, t.title, t.artist, t.mime || "audio/mpeg");
      patchTrack(t.id, { published: true });
      refreshShared();
    } catch (e) {
      console.error("publish failed", e);
    }
  }

  /** 流式播放一首共享曲目（边下边播：分片按需从 peer 拉取）。 */
  async function onStreamPlay(s: SharedTrack) {
    if (!isTauri()) return;
    try {
      const url = await prepareStream(s.manifest, s.mime || "audio/mpeg");
      const t: Track = {
        id: nextId.current++,
        title: s.title,
        artist: s.artist,
        url,
        manifest: s.manifest,
        mime: s.mime,
        duration: "--:--",
        status: "ready",
        published: true,
      };
      setView("library");
      setCurrent(t);
      player.load(url, true);
    } catch (e) {
      console.error("stream play failed", e);
      alert(`流式播放失败：${e}`);
    }
  }

  /** 从共享曲库下载一首曲目完整收藏到本地（分片从其他节点 P2P 拉取）。 */
  async function onDownload(s: SharedTrack) {
    if (!isTauri()) return;
    const hash = s.manifest.track_hash;
    setDownloading((prev) => new Set(prev).add(hash));
    try {
      const result = await downloadTrack(
        s.manifest,
        s.title,
        s.artist,
        s.mime || "audio/mpeg"
      );
      const blob = new Blob([result.data], { type: s.mime || "audio/mpeg" });
      const url = URL.createObjectURL(blob);
      const t: Track = {
        id: nextId.current++,
        title: s.title,
        artist: s.artist,
        url,
        manifest: s.manifest,
        mime: s.mime,
        duration: "--:--",
        status: "ready",
        published: true,
      };
      setTracks((prev) => [t, ...prev]);
      setView("library");
      playTrack(t);
      console.log(
        `downloaded ${s.title}: fetched ${result.fetched}, cached ${result.cached}`
      );
    } catch (e) {
      console.error("download failed", e);
      alert(`下载失败：${e}`);
    } finally {
      setDownloading((prev) => {
        const next = new Set(prev);
        next.delete(hash);
        return next;
      });
    }
  }

  /** 复制曲目资源标识符（music://track/<hash>）到剪贴板。 */
  async function onCopyLink(t: Track) {
    if (!t.manifest) return;
    const uri = makeTrackUri(t.manifest.track_hash);
    try {
      await navigator.clipboard.writeText(uri);
    } catch {
      // 退化方案：prompt 出来让用户自己复制
      window.prompt("复制曲目链接", uri);
    }
  }

  /** 从曲库移除一首曲目（分片留给缓存淘汰按需回收）。 */
  async function onRemove(t: Track) {
    if (!t.manifest || !isTauri()) return;
    try {
      await removeFromLibrary(t.manifest.track_hash);
      setTracks((prev) => prev.filter((x) => x.id !== t.id));
      // 正在播的被移除时停下并清空当前曲目。
      setCurrent((c) => {
        if (c?.id === t.id) {
          player.stop();
          return null;
        }
        return c;
      });
    } catch (e) {
      console.error("remove failed", e);
    }
  }

  /** 处理粘贴链接：解析 → 查 Tracker → 走流式播放（与点「播放」一致）。 */
  async function onPasteLink() {
    setPasteError(null);
    const hash = parseTrackInput(pasteInput);
    if (!hash) {
      setPasteError("格式不对，需要 music://track/<64位hash> 或 64位 hash 本身");
      return;
    }
    if (!isTauri()) {
      setPasteError("需要 Tauri 客户端");
      return;
    }
    try {
      const item = await lookupTrack(hash);
      if (!item) {
        setPasteError("Tracker 上找不到这首曲目（没人发布过）");
        return;
      }
      setPasteInput("");
      // 复用流式播放路径
      await onStreamPlay(item);
    } catch (e) {
      console.error("paste link failed", e);
      setPasteError(String(e));
    }
  }

  const played = player.duration ? (player.currentTime / player.duration) * 100 : 0;
  const buffered = player.duration ? (player.buffered / player.duration) * 100 : 0;
  const localHashes = new Set(
    tracks.filter((t) => t.manifest).map((t) => t.manifest!.track_hash)
  );

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
          <NavItem
            icon={<Library className="size-4" />}
            label="曲库"
            active={view === "library"}
            onClick={() => setView("library")}
          />
          <NavItem icon={<ListMusic className="size-4" />} label="播放列表" />
          <NavItem
            icon={<Users className="size-4" />}
            label="节点网络"
            active={view === "network"}
            onClick={() => setView("network")}
          />

          <div className="mt-auto space-y-2">
            <div className="rounded-md bg-secondary/50 p-3 text-xs text-muted-foreground">
              <div className="mb-1.5 flex items-center gap-1.5 font-medium text-foreground">
                <LinkIcon className="size-3.5" /> 粘贴链接播放
              </div>
              <div className="flex gap-1">
                <input
                  type="text"
                  value={pasteInput}
                  onChange={(e) => {
                    setPasteInput(e.target.value);
                    setPasteError(null);
                  }}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") void onPasteLink();
                  }}
                  placeholder="music://track/..."
                  className="min-w-0 flex-1 rounded border border-input bg-background px-2 py-1 text-xs text-foreground placeholder:text-muted-foreground/50 focus:outline-none focus:ring-1 focus:ring-ring"
                />
                <Button
                  size="icon"
                  variant="secondary"
                  className="size-7"
                  onClick={() => void onPasteLink()}
                  title="查找并播放"
                >
                  <ClipboardPaste className="size-3.5" />
                </Button>
              </div>
              {pasteError && (
                <div className="mt-1.5 text-[10px] text-destructive">{pasteError}</div>
              )}
            </div>

            <div className="space-y-2 rounded-md bg-secondary/50 p-3 text-xs text-muted-foreground">
            <div className="flex items-center gap-1.5 font-medium text-foreground">
              <Share2 className="size-3.5" /> P2P 状态
            </div>
            {status ? (
              <>
                <div className="flex items-center justify-between">
                  <span>Tracker</span>
                  <span
                    className={cn(
                      "flex items-center gap-1 font-medium",
                      status.tracker_online ? "text-emerald-400" : "text-destructive"
                    )}
                  >
                    <CircleDot className="size-3" />
                    {status.tracker_online ? "在线" : "离线"}
                  </span>
                </div>
                <StatusRow label="本地分片" value={`${status.owned_chunks}`} />
                {cache && (
                  <StatusRow
                    label="缓存占用"
                    value={`${fmtBytes(cache.bytes)} / ${fmtBytes(cache.limit)}`}
                  />
                )}
                <div className="truncate pt-1 text-[10px] text-muted-foreground/70">
                  peer: {status.peer_id.slice(0, 8)}… @ {status.chunk_addr}
                </div>
              </>
            ) : (
              <div className="text-[11px] text-muted-foreground/70">
                {isTauri() ? "连接中…" : "浏览器预览（无后端）"}
              </div>
            )}
            </div>
          </div>
        </aside>

        {/* content */}
        <main className="flex-1 overflow-y-auto p-6">
          {view === "library" ? (
            <LibraryView
              tracks={tracks}
              current={current}
              onPlay={playTrack}
              onPublish={onPublish}
              onCopyLink={onCopyLink}
              onRemove={onRemove}
              onImport={() => fileInputRef.current?.click()}
            />
          ) : (
            <NetworkView
              shared={shared}
              localHashes={localHashes}
              downloading={downloading}
              trackerOnline={status?.tracker_online ?? false}
              onRefresh={refreshShared}
              onStreamPlay={onStreamPlay}
              onDownload={onDownload}
            />
          )}
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

function LibraryView({
  tracks,
  current,
  onPlay,
  onPublish,
  onCopyLink,
  onRemove,
  onImport,
}: {
  tracks: Track[];
  current: Track | null;
  onPlay: (t: Track) => void;
  onPublish: (t: Track) => void;
  onCopyLink: (t: Track) => void;
  onRemove: (t: Track) => void;
  onImport: () => void;
}) {
  return (
    <>
      <div className="mb-6 flex items-start justify-between">
        <div>
          <h1 className="text-2xl font-bold">曲库</h1>
          <p className="text-sm text-muted-foreground">
            边下边播 · 多源加速 · 人越多播放越流畅
          </p>
        </div>
        <Button onClick={onImport}>
          <Plus /> 导入音乐
        </Button>
      </div>

      <div className="overflow-hidden rounded-lg border border-border">
        {tracks.length === 0 ? (
          <div className="p-10 text-center text-sm text-muted-foreground">
            曲库为空。点右上「导入音乐」添加本地文件，或去「节点网络」下载别人分享的曲目。
          </div>
        ) : (
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
              <th className="px-4 py-2.5 text-right">操作</th>
            </tr>
          </thead>
          <tbody>
            {tracks.map((t, i) => (
              <tr
                key={t.id}
                onClick={() => t.url && onPlay(t)}
                className={cn(
                  "border-t border-border transition-colors",
                  t.url ? "cursor-pointer hover:bg-accent/50" : "cursor-default",
                  current?.id === t.id && "bg-accent/40",
                  !t.url && "opacity-60"
                )}
                title={
                  t.manifest
                    ? `track ${t.manifest.track_hash.slice(0, 12)}… · ${fmtBytes(
                        t.manifest.total_size
                      )}`
                    : ""
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
                <td className="px-4 py-3 text-right">
                  <div className="flex items-center justify-end gap-1">
                    {t.manifest && (
                      <Button
                        variant="ghost"
                        size="icon"
                        className="size-7"
                        title="复制曲目链接"
                        onClick={(e) => {
                          e.stopPropagation();
                          onCopyLink(t);
                        }}
                      >
                        <LinkIcon className="size-3.5" />
                      </Button>
                    )}
                    {t.manifest &&
                      (t.published ? (
                        <span className="inline-flex items-center gap-1 text-xs text-emerald-400">
                          <CheckCircle2 className="size-3.5" /> 已发布
                        </span>
                      ) : (
                        <Button
                          variant="ghost"
                          size="sm"
                          onClick={(e) => {
                            e.stopPropagation();
                            onPublish(t);
                          }}
                        >
                          <Upload className="size-3.5" /> 发布
                        </Button>
                      ))}
                    {t.manifest && (
                      <Button
                        variant="ghost"
                        size="icon"
                        className="size-7 text-muted-foreground hover:text-destructive"
                        title="从曲库移除"
                        onClick={(e) => {
                          e.stopPropagation();
                          onRemove(t);
                        }}
                      >
                        <Trash2 className="size-3.5" />
                      </Button>
                    )}
                  </div>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        )}
      </div>
    </>
  );
}

function NetworkView({
  shared,
  localHashes,
  downloading,
  trackerOnline,
  onRefresh,
  onStreamPlay,
  onDownload,
}: {
  shared: SharedTrack[];
  localHashes: Set<string>;
  downloading: Set<string>;
  trackerOnline: boolean;
  onRefresh: () => void;
  onStreamPlay: (s: SharedTrack) => void;
  onDownload: (s: SharedTrack) => void;
}) {
  return (
    <>
      <div className="mb-6 flex items-start justify-between">
        <div>
          <h1 className="text-2xl font-bold">节点网络</h1>
          <p className="text-sm text-muted-foreground">
            共享曲库 —— 点「播放」即边下边播，分片按需从持有者 P2P 拉取
          </p>
        </div>
        <Button variant="outline" onClick={onRefresh}>
          <RefreshCw /> 刷新
        </Button>
      </div>

      {!trackerOnline && (
        <div className="mb-4 rounded-md border border-destructive/40 bg-destructive/10 p-3 text-sm text-destructive">
          Tracker 离线。请先启动 Tracker 服务：
          <code className="ml-1 rounded bg-background/50 px-1.5 py-0.5 text-xs">
            music --tracker
          </code>
        </div>
      )}

      {shared.length === 0 ? (
        <div className="rounded-lg border border-dashed border-border p-10 text-center text-sm text-muted-foreground">
          共享曲库为空。在「曲库」里导入一首歌并点「发布」，或从另一个客户端发布。
        </div>
      ) : (
        <div className="overflow-hidden rounded-lg border border-border">
          <table className="w-full text-sm">
            <thead className="bg-secondary/40 text-left text-xs text-muted-foreground">
              <tr>
                <th className="px-4 py-2.5">标题</th>
                <th className="px-4 py-2.5">艺术家</th>
                <th className="px-4 py-2.5">
                  <span className="flex items-center gap-1">
                    <Boxes className="size-3.5" /> 分片
                  </span>
                </th>
                <th className="px-4 py-2.5">大小</th>
                <th className="px-4 py-2.5 text-right">操作</th>
              </tr>
            </thead>
            <tbody>
              {shared.map((s) => {
                const have = localHashes.has(s.manifest.track_hash);
                const busy = downloading.has(s.manifest.track_hash);
                return (
                  <tr key={s.manifest.track_hash} className="border-t border-border">
                    <td className="px-4 py-3 font-medium">{s.title}</td>
                    <td className="px-4 py-3 text-muted-foreground">{s.artist}</td>
                    <td className="px-4 py-3 tabular-nums text-muted-foreground">
                      {s.manifest.chunks.length}
                    </td>
                    <td className="px-4 py-3 text-muted-foreground">
                      {fmtBytes(s.manifest.total_size)}
                    </td>
                    <td className="px-4 py-3">
                      <div className="flex items-center justify-end gap-2">
                        <Button size="sm" onClick={() => onStreamPlay(s)}>
                          <Play className="size-3.5" /> 播放
                        </Button>
                        {have ? (
                          <span className="inline-flex items-center gap-1 text-xs text-emerald-400">
                            <CheckCircle2 className="size-3.5" /> 已缓存
                          </span>
                        ) : (
                          <Button
                            variant="ghost"
                            size="sm"
                            disabled={busy}
                            onClick={() => onDownload(s)}
                            title="完整下载到本地"
                          >
                            {busy ? (
                              <Loader2 className="size-3.5 animate-spin" />
                            ) : (
                              <Download className="size-3.5" />
                            )}
                            {busy ? "下载中" : "下载"}
                          </Button>
                        )}
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </>
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
  onClick,
}: {
  icon: React.ReactNode;
  label: string;
  active?: boolean;
  onClick?: () => void;
}) {
  return (
    <button
      onClick={onClick}
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

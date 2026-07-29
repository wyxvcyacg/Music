import { useRef, useState } from "react";
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
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { usePlayer, fmtTime } from "@/hooks/usePlayer";

type Track = {
  id: number;
  title: string;
  artist: string;
  /** 本地导入的音源 URL（object URL）。示例曲目为 undefined，不可播放。 */
  url?: string;
  /** 时长文本；本地导入曲目载入元数据后填充。 */
  duration: string;
  peers: number;
};

const DEMO_TRACKS: Track[] = [
  { id: 1, title: "Midnight City Lights", artist: "Neon Drive", duration: "3:42", peers: 12 },
  { id: 2, title: "Echoes in the Rain", artist: "Lo-Fi Collective", duration: "4:15", peers: 8 },
  { id: 3, title: "Digital Horizon", artist: "Synthwave Kid", duration: "5:01", peers: 24 },
  { id: 4, title: "Ocean of Static", artist: "Ambient Waves", duration: "6:20", peers: 3 },
];

function App() {
  const player = usePlayer();
  const [tracks, setTracks] = useState<Track[]>(DEMO_TRACKS);
  const [current, setCurrent] = useState<Track | null>(null);
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const nextId = useRef(100);

  function playTrack(t: Track) {
    setCurrent(t);
    if (t.url) player.load(t.url, true);
  }

  function onImportFiles(files: FileList | null) {
    if (!files || files.length === 0) return;
    const imported: Track[] = Array.from(files).map((f) => ({
      id: nextId.current++,
      title: f.name.replace(/\.[^.]+$/, ""),
      artist: "本地导入",
      url: URL.createObjectURL(f),
      duration: "--:--",
      peers: 1, // 导入后本节点即持有 → 1 个来源
    }));
    setTracks((prev) => [...imported, ...prev]);
    // 自动播放第一首导入的。
    playTrack(imported[0]);
  }

  const played = player.duration ? (player.currentTime / player.duration) * 100 : 0;
  const buffered = player.duration ? (player.buffered / player.duration) * 100 : 0;

  return (
    <div className="dark flex h-screen flex-col bg-background text-foreground">
      {/* hidden file input for importing local audio */}
      <input
        ref={fileInputRef}
        type="file"
        accept="audio/*"
        multiple
        hidden
        onChange={(e) => onImportFiles(e.target.files)}
      />

      {/* main area */}
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

          <div className="mt-auto rounded-md bg-secondary/50 p-3 text-xs text-muted-foreground">
            <div className="mb-1 flex items-center gap-1.5 font-medium text-foreground">
              <Share2 className="size-3.5" /> P2P 状态
            </div>
            已连接 <span className="text-foreground">14</span> 个节点 · 上传{" "}
            <span className="text-foreground">1.2 MB/s</span>
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
                    onClick={() => playTrack(t)}
                    className={cn(
                      "cursor-pointer border-t border-border transition-colors hover:bg-accent/50",
                      current?.id === t.id && "bg-accent/40",
                      !t.url && "opacity-60"
                    )}
                    title={t.url ? "" : "示例曲目（无音源）—— 点「导入音乐」添加可播放文件"}
                  >
                    <td className="px-4 py-3 text-muted-foreground">{i + 1}</td>
                    <td className="px-4 py-3 font-medium">{t.title}</td>
                    <td className="px-4 py-3 text-muted-foreground">{t.artist}</td>
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
          {/* now playing */}
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

          {/* controls + progress */}
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
                {/* buffered */}
                <div
                  className="absolute inset-y-0 left-0 bg-muted-foreground/40"
                  style={{ width: `${buffered}%` }}
                />
                {/* played */}
                <div
                  className="absolute inset-y-0 left-0 bg-primary"
                  style={{ width: `${played}%` }}
                />
              </div>
              <span className="w-9 tabular-nums">{fmtTime(player.duration)}</span>
            </div>
          </div>

          {/* volume */}
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

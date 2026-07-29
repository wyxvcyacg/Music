import { useState } from "react";
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
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

type Track = {
  id: number;
  title: string;
  artist: string;
  duration: string;
  peers: number;
};

const TRACKS: Track[] = [
  { id: 1, title: "Midnight City Lights", artist: "Neon Drive", duration: "3:42", peers: 12 },
  { id: 2, title: "Echoes in the Rain", artist: "Lo-Fi Collective", duration: "4:15", peers: 8 },
  { id: 3, title: "Digital Horizon", artist: "Synthwave Kid", duration: "5:01", peers: 24 },
  { id: 4, title: "Ocean of Static", artist: "Ambient Waves", duration: "6:20", peers: 3 },
  { id: 5, title: "Retro Future", artist: "Neon Drive", duration: "3:58", peers: 17 },
  { id: 6, title: "Quiet Streets", artist: "Lo-Fi Collective", duration: "2:47", peers: 5 },
];

function App() {
  const [current, setCurrent] = useState<Track>(TRACKS[0]);
  const [playing, setPlaying] = useState(false);
  const [progress] = useState(38); // % — mock buffer/play position

  return (
    <div className="dark flex h-screen flex-col bg-background text-foreground">
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
          <div className="mb-6">
            <h1 className="text-2xl font-bold">曲库</h1>
            <p className="text-sm text-muted-foreground">
              边下边播 · 多源加速 · 人越多播放越流畅
            </p>
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
                {TRACKS.map((t, i) => (
                  <tr
                    key={t.id}
                    onClick={() => {
                      setCurrent(t);
                      setPlaying(true);
                    }}
                    className={cn(
                      "cursor-pointer border-t border-border transition-colors hover:bg-accent/50",
                      current.id === t.id && "bg-accent/40"
                    )}
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
              <div className="truncate text-sm font-medium">{current.title}</div>
              <div className="truncate text-xs text-muted-foreground">
                {current.artist}
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
                onClick={() => setPlaying((p) => !p)}
              >
                {playing ? <Pause /> : <Play />}
              </Button>
              <Button variant="ghost" size="icon">
                <SkipForward />
              </Button>
              <Button variant="ghost" size="icon" className="text-muted-foreground">
                <Repeat />
              </Button>
            </div>
            <div className="flex w-full max-w-xl items-center gap-2 text-xs text-muted-foreground">
              <span>1:24</span>
              <div className="relative h-1 flex-1 overflow-hidden rounded-full bg-secondary">
                {/* buffered */}
                <div
                  className="absolute inset-y-0 left-0 bg-muted-foreground/40"
                  style={{ width: `${Math.min(progress + 25, 100)}%` }}
                />
                {/* played */}
                <div
                  className="absolute inset-y-0 left-0 bg-primary"
                  style={{ width: `${progress}%` }}
                />
              </div>
              <span>{current.duration}</span>
            </div>
          </div>

          {/* volume */}
          <div className="flex w-60 items-center justify-end gap-2">
            <Volume2 className="size-4 text-muted-foreground" />
            <div className="h-1 w-24 overflow-hidden rounded-full bg-secondary">
              <div className="h-full w-2/3 bg-muted-foreground" />
            </div>
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

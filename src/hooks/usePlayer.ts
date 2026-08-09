import { useCallback, useEffect, useRef, useState } from "react";

/**
 * usePlayer —— 封装一个 HTMLAudioElement，管理真实音频播放状态。
 *
 * 阶段一：直接播放 URL（本地文件的 object URL 或远程地址）。
 * 阶段二起：src 会换成由 ChunkStore 重组、经 MediaSource 边下边播的流，
 * 但这个 hook 对外暴露的接口（play/pause/seek/进度）保持不变。
 */
export type PlayerState = {
  playing: boolean;
  /** 当前播放位置（秒） */
  currentTime: number;
  /** 总时长（秒），未知时为 0 */
  duration: number;
  /** 已缓冲到的秒数（体现"边下边播"的缓冲区间） */
  buffered: number;
  /** 音量 0–1 */
  volume: number;
};

export function usePlayer() {
  const audioRef = useRef<HTMLAudioElement | null>(null);
  const [state, setState] = useState<PlayerState>({
    playing: false,
    currentTime: 0,
    duration: 0,
    buffered: 0,
    volume: 1,
  });

  // 惰性创建单例 audio 元素。
  if (audioRef.current === null && typeof Audio !== "undefined") {
    audioRef.current = new Audio();
  }

  useEffect(() => {
    const audio = audioRef.current;
    if (!audio) return;

    const onTime = () =>
      setState((s) => ({ ...s, currentTime: audio.currentTime }));
    const onMeta = () =>
      setState((s) => ({ ...s, duration: audio.duration || 0 }));
    const onPlay = () => setState((s) => ({ ...s, playing: true }));
    const onPause = () => setState((s) => ({ ...s, playing: false }));
    const onEnded = () => setState((s) => ({ ...s, playing: false }));
    const onProgress = () => {
      // 取最后一个缓冲区间的末尾，作为"已缓冲到"的位置。
      const b = audio.buffered;
      const end = b.length ? b.end(b.length - 1) : 0;
      setState((s) => ({ ...s, buffered: end }));
    };

    audio.addEventListener("timeupdate", onTime);
    audio.addEventListener("loadedmetadata", onMeta);
    audio.addEventListener("durationchange", onMeta);
    audio.addEventListener("play", onPlay);
    audio.addEventListener("pause", onPause);
    audio.addEventListener("ended", onEnded);
    audio.addEventListener("progress", onProgress);

    return () => {
      audio.removeEventListener("timeupdate", onTime);
      audio.removeEventListener("loadedmetadata", onMeta);
      audio.removeEventListener("durationchange", onMeta);
      audio.removeEventListener("play", onPlay);
      audio.removeEventListener("pause", onPause);
      audio.removeEventListener("ended", onEnded);
      audio.removeEventListener("progress", onProgress);
    };
  }, []);

  /** 载入一个新的音源 URL 并（可选）立即播放。 */
  const load = useCallback((src: string, autoplay = true) => {
    const audio = audioRef.current;
    if (!audio) return;
    audio.src = src;
    audio.load();
    if (autoplay) void audio.play().catch(() => {});
  }, []);

  const toggle = useCallback(() => {
    const audio = audioRef.current;
    if (!audio || !audio.src) return;
    if (audio.paused) void audio.play().catch(() => {});
    else audio.pause();
  }, []);

  const seek = useCallback((time: number) => {
    const audio = audioRef.current;
    if (!audio) return;
    audio.currentTime = time;
    setState((s) => ({ ...s, currentTime: time }));
  }, []);

  const setVolume = useCallback((v: number) => {
    const audio = audioRef.current;
    if (!audio) return;
    const clamped = Math.min(1, Math.max(0, v));
    audio.volume = clamped;
    setState((s) => ({ ...s, volume: clamped }));
  }, []);

  /** 停止播放并清空音源（例如当前曲目被移除时）。 */
  const stop = useCallback(() => {
    const audio = audioRef.current;
    if (!audio) return;
    audio.pause();
    audio.removeAttribute("src");
    audio.load();
    setState((s) => ({
      ...s,
      playing: false,
      currentTime: 0,
      duration: 0,
      buffered: 0,
    }));
  }, []);

  return { ...state, load, toggle, seek, setVolume, stop };
}

/** 秒 → m:ss */
export function fmtTime(sec: number): string {
  if (!isFinite(sec) || sec < 0) return "0:00";
  const m = Math.floor(sec / 60);
  const s = Math.floor(sec % 60);
  return `${m}:${s.toString().padStart(2, "0")}`;
}

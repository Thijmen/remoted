export interface Rect {
  left: number;
  top: number;
  width: number;
  height: number;
}

/** Wire coordinates span 0..=65535 over the video frame. */
export const NORM_MAX = 0xffff;

/** Content box of a `object-fit: contain` video (`videoW`×`videoH`) inside `el` (letterboxing). */
export function containRect(el: Rect, videoW: number, videoH: number): Rect {
  if (videoW <= 0 || videoH <= 0 || el.width <= 0 || el.height <= 0) return el;
  const scale = Math.min(el.width / videoW, el.height / videoH);
  const width = videoW * scale;
  const height = videoH * scale;
  return { left: el.left + (el.width - width) / 2, top: el.top + (el.height - height) / 2, width, height };
}

const clamp01 = (v: number) => (v <= 0 ? 0 : v >= 1 ? 1 : v);

/**
 * Map a client-space point to normalised video coordinates. Points over the letterbox bars
 * clamp to the nearest edge, so the remote cursor can always reach the screen edges.
 */
export function normalizePoint(
  clientX: number,
  clientY: number,
  el: Rect,
  videoW: number,
  videoH: number,
): { x: number; y: number } {
  const r = containRect(el, videoW, videoH);
  const fx = r.width > 0 ? clamp01((clientX - r.left) / r.width) : 0;
  const fy = r.height > 0 ? clamp01((clientY - r.top) / r.height) : 0;
  return { x: Math.round(fx * NORM_MAX), y: Math.round(fy * NORM_MAX) };
}

/** Normalised coordinates back to video pixels. */
export function toVideoPixels(x: number, y: number, videoW: number, videoH: number): { x: number; y: number } {
  return { x: (x / NORM_MAX) * (videoW - 1), y: (y / NORM_MAX) * (videoH - 1) };
}

// Which remote files the file browser can preview, judged by their extension.

export type MediaKind = 'image' | 'audio' | 'video';

export interface MediaType {
  kind: MediaKind;
  /** The MIME type the page gives the fetched bytes (browsers need it for some formats). */
  mime: string;
}

const TYPES: Record<string, MediaType> = {};
const add = (kind: MediaKind, mime: string, ...exts: string[]) => {
  for (const ext of exts) TYPES[ext] = { kind, mime };
};

add('image', 'image/png', 'png', 'apng');
add('image', 'image/jpeg', 'jpg', 'jpeg', 'jpe', 'jfif');
add('image', 'image/gif', 'gif');
add('image', 'image/webp', 'webp');
add('image', 'image/avif', 'avif');
add('image', 'image/bmp', 'bmp');
add('image', 'image/x-icon', 'ico');
// Shown in an <img>, where an SVG's scripts never run.
add('image', 'image/svg+xml', 'svg');
// Safari only; other browsers show the "cannot show" message.
add('image', 'image/heic', 'heic');
add('image', 'image/heif', 'heif');
add('image', 'image/tiff', 'tif', 'tiff');

add('audio', 'audio/mpeg', 'mp3');
add('audio', 'audio/mp4', 'm4a', 'aac', 'm4b');
add('audio', 'audio/wav', 'wav');
add('audio', 'audio/ogg', 'ogg', 'oga', 'opus');
add('audio', 'audio/flac', 'flac');
add('audio', 'audio/webm', 'weba');
add('audio', 'audio/aiff', 'aif', 'aiff');

add('video', 'video/mp4', 'mp4', 'm4v');
add('video', 'video/quicktime', 'mov');
add('video', 'video/webm', 'webm');
add('video', 'video/ogg', 'ogv');
add('video', 'video/x-matroska', 'mkv');

/** The preview type of a file named `name`, or null when it cannot be previewed. */
export function mediaType(name: string): MediaType | null {
  const dot = name.lastIndexOf('.');
  if (dot <= 0 || dot === name.length - 1) return null;
  return TYPES[name.slice(dot + 1).toLowerCase()] ?? null;
}

/** Above this size a preview asks first: it fetches the whole file before showing it. */
export const PREVIEW_ASK_BYTES = 200_000_000;

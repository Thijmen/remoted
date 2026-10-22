/** Small line icons (24×24 viewBox, stroked with `currentColor`). */
const PATHS = {
  monitor: ['M3 4.5h18v12H3z', 'M8.5 20.5h7', 'M12 16.5v4'],
  fullscreen: ['M4 9V4h5', 'M20 9V4h-5', 'M4 15v5h5', 'M20 15v5h-5'],
  pointer: ['M5 3.5l13 6.2-5.6 1.8-2.2 5.7z', 'M12.6 11.7l5.4 5.4'],
  stats: ['M5 20V11', 'M12 20V5', 'M19 20v-6'],
  settings: ['M4 7h10', 'M18 7h2', 'M4 17h3', 'M11 17h9', 'M16 4.5v5', 'M9 14.5v5'],
  timer: ['M12 21a8 8 0 1 0 0-16 8 8 0 0 0 0 16z', 'M12 9v4l2.5 2', 'M10 2.5h4'],
  download: ['M12 4v11', 'M7 10.5l5 5 5-5', 'M5 20h14'],
  power: ['M12 3.5v8', 'M7.1 6.6a7.5 7.5 0 1 0 9.8 0'],
  close: ['M6 6l12 12', 'M18 6L6 18'],
  chevronDown: ['M6 9.5l6 6 6-6'],
  back: ['M15 5.5L8.5 12l6.5 6.5'],
  plus: ['M12 5v14', 'M5 12h14'],
  minus: ['M5 12h14'],
  terminal: ['M3.5 5h17v14h-17z', 'M7.5 9.5l3 2.5-3 2.5', 'M12.5 15h4'],
  folder: ['M3.5 6.5a2 2 0 0 1 2-2h4l2 2.5h7a2 2 0 0 1 2 2v8.5a2 2 0 0 1-2 2h-13a2 2 0 0 1-2-2z'],
  folderPlus: [
    'M3.5 6.5a2 2 0 0 1 2-2h4l2 2.5h7a2 2 0 0 1 2 2v8.5a2 2 0 0 1-2 2h-13a2 2 0 0 1-2-2z',
    'M12 10.5v6',
    'M9 13.5h6',
  ],
  file: ['M6.5 3.5h7l4.5 4.5v12.5h-11.5z', 'M13.5 3.5V8h4.5'],
  upload: ['M12 15V4', 'M7 8.5l5-5 5 5', 'M5 20h14'],
  trash: ['M4.5 6.5h15', 'M9.5 6.5V4.5h5v2', 'M6.5 6.5l1 13h9l1-13', 'M10 10.5v5.5', 'M14 10.5v5.5'],
  refresh: ['M19.5 12a7.5 7.5 0 1 1-2.2-5.3', 'M19.5 4.5v4h-4'],
  eye: [
    'M2.5 12s3.5-6.5 9.5-6.5 9.5 6.5 9.5 6.5-3.5 6.5-9.5 6.5S2.5 12 2.5 12z',
    'M12 14.8a2.8 2.8 0 1 0 0-5.6 2.8 2.8 0 0 0 0 5.6z',
  ],
  eyeOff: [
    'M4 4l16 16',
    'M9.9 5.8A9.6 9.6 0 0 1 12 5.5c6 0 9.5 6.5 9.5 6.5a17 17 0 0 1-2.6 3.4',
    'M6.4 7.6C3.9 9.4 2.5 12 2.5 12s3.5 6.5 9.5 6.5c1.6 0 3-.5 4.2-1.1',
  ],
  up: ['M12 19.5V5', 'M6 11l6-6 6 6'],
  forward: ['M9 5.5l6.5 6.5L9 18.5'],
  home: ['M4 11l8-6.5 8 6.5', 'M6 9.5v10h12v-10', 'M10 19.5v-5h4v5'],
  documents: ['M6.5 3.5h7l4.5 4.5v12.5h-11.5z', 'M13.5 3.5V8h4.5', 'M9 12.5h6', 'M9 16h6'],
  drive: ['M3.5 13.5l2.5-8h12l2.5 8', 'M3.5 13.5h17v5h-17z', 'M16.5 16h.01'],
  pause: ['M8.5 5.5v13', 'M15.5 5.5v13'],
  play: ['M7.5 5l11 7-11 7z'],
  retry: ['M4.5 12a7.5 7.5 0 1 0 2.2-5.3', 'M4.5 4.5v4h4'],
  check: ['M5 12.5l4.5 4.5L19 7.5'],
  alert: ['M12 4l9 16H3z', 'M12 10v4.5', 'M12 17.5h.01'],
  search: ['M10.5 17.5a7 7 0 1 0 0-14 7 7 0 0 0 0 14z', 'M15.5 15.5l5 5'],
  pencil: ['M15 5l4 4', 'M4.5 19.5l1-4.5L16 4.5l3.5 3.5L9 18.5z'],
  transfer: ['M7 4.5v15', 'M3.5 8L7 4.5 10.5 8', 'M17 19.5v-15', 'M13.5 16l3.5 3.5 3.5-3.5'],
  link: ['M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1', 'M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1'],
  copy: ['M8.5 8.5h11v11h-11z', 'M15.5 8.5v-4h-11v11h4'],
} as const;

export type IconName = keyof typeof PATHS;

const SVG_NS = 'http://www.w3.org/2000/svg';

export function icon(name: IconName, size = 18): SVGSVGElement {
  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('width', String(size));
  svg.setAttribute('height', String(size));
  svg.setAttribute('fill', 'none');
  svg.setAttribute('stroke', 'currentColor');
  svg.setAttribute('stroke-width', '1.8');
  svg.setAttribute('stroke-linecap', 'round');
  svg.setAttribute('stroke-linejoin', 'round');
  svg.setAttribute('aria-hidden', 'true');
  svg.classList.add('icon');
  for (const d of PATHS[name]) {
    const path = document.createElementNS(SVG_NS, 'path');
    path.setAttribute('d', d);
    svg.appendChild(path);
  }
  return svg;
}

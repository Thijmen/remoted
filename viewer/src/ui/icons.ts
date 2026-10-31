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
  arrowRight: ['M4.5 12h15', 'M13.5 6l6 6-6 6'],
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
  activity: ['M3 12h4l2.5-6.5 5 13 2.5-6.5h4'],
  gauge: ['M12 14l3.5-3.5', 'M4.2 18a9 9 0 1 1 15.6 0'],
  keyboard: ['M3.5 6.5h17v11h-17z', 'M7 10h.01', 'M10.5 10h.01', 'M14 10h.01', 'M17 10h.01', 'M8 14h8'],
  live: [
    'M12 13.5a1.5 1.5 0 1 0 0-3 1.5 1.5 0 0 0 0 3z',
    'M8.3 15.7a5.3 5.3 0 0 1 0-7.4',
    'M15.7 8.3a5.3 5.3 0 0 1 0 7.4',
    'M5.4 18.6a9.4 9.4 0 0 1 0-13.2',
    'M18.6 5.4a9.4 9.4 0 0 1 0 13.2',
  ],
  update: ['M12 20.5a8.5 8.5 0 1 0 0-17 8.5 8.5 0 0 0 0 17z', 'M12 16V8', 'M8.5 11.5L12 8l3.5 3.5'],
  key: ['M8 19.5a4.5 4.5 0 1 0 0-9 4.5 4.5 0 0 0 0 9z', 'M11.2 11.8l8.3-8.3', 'M16.5 6.5l2.5 2.5', 'M14 9l2 2'],
  info: ['M12 20.5a8.5 8.5 0 1 0 0-17 8.5 8.5 0 0 0 0 17z', 'M12 11v5', 'M12 8h.01'],
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

/** Filled marks for the operating systems the agent runs on (24×24). */
const OS_MARKS = {
  windows: 'M3 5.6l7.4-1v7H3zM11.4 4.4L21 3v8.6h-9.6zM3 12.4h7.4v7L3 18.4zM11.4 12.4H21V21l-9.6-1.4z',
  mac: 'M16.4 12.7c0-2.5 2-3.7 2.1-3.8-1.2-1.7-3-1.9-3.6-2-1.5-.2-3 .9-3.8.9-.8 0-2-.9-3.3-.9-1.7 0-3.3 1-4.2 2.5-1.8 3.1-.5 7.7 1.3 10.2.9 1.2 1.9 2.6 3.2 2.6 1.3-.1 1.8-.8 3.3-.8 1.6 0 2 .8 3.4.8 1.4 0 2.3-1.3 3.1-2.5 1-1.4 1.4-2.8 1.4-2.9-.1 0-2.9-1.1-2.9-4.1zM13.9 5.3c.7-.9 1.2-2.1 1.1-3.3-1 0-2.3.7-3 1.6-.7.8-1.2 2-1.1 3.2 1.2.1 2.3-.6 3-1.5z',
} as const;

/** An operating system's mark; the stroked monitor or terminal icon for the rest. */
export function osMark(family: 'mac' | 'windows' | 'linux' | 'other', size = 18): SVGSVGElement {
  if (family === 'linux') return icon('terminal', size);
  if (family === 'other') return icon('monitor', size);
  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('width', String(size));
  svg.setAttribute('height', String(size));
  svg.setAttribute('fill', 'currentColor');
  svg.setAttribute('aria-hidden', 'true');
  svg.classList.add('icon');
  const path = document.createElementNS(SVG_NS, 'path');
  path.setAttribute('d', OS_MARKS[family]);
  svg.appendChild(path);
  return svg;
}

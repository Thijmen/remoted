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

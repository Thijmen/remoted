// Latency target, opened ON the host: a large ms counter (for 240 fps camera tests) and a box
// that flips colour on every F13 keydown or click (for the viewer's automated input→photon test).
import '../ui/style.css';
import { byId, h } from '../ui/dom';

const counter = h('div', { className: 'counter' }, '0');
const box = h('div', { className: 'box', title: 'Rest the remote cursor here' });
const flips = h('div', { className: 'muted' }, 'flips: 0');
let n = 0;

byId('app').append(
  h(
    'main',
    { className: 'latency' },
    counter,
    box,
    flips,
    h(
      'div',
      { className: 'muted', style: 'max-width:520px;text-align:center' },
      'Keep this window focused on the host. In the viewer, rest the cursor on the box and press Ctrl+Alt+Shift+T. ' +
        'For camera tests, film this counter next to the viewer.',
    ),
  ),
);

function flip(): void {
  box.classList.toggle('on');
  flips.textContent = `flips: ${++n}`;
}

window.addEventListener('keydown', (e) => {
  if (e.code === 'F13' && !e.repeat) {
    e.preventDefault();
    flip();
  }
});
box.addEventListener('pointerdown', flip);

const tick = () => {
  counter.textContent = Math.floor(performance.now()).toString();
  requestAnimationFrame(tick);
};
requestAnimationFrame(tick);

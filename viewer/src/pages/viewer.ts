import '../ui/style.css';
import { byId, h } from '../ui/dom';
import { ViewerApp, machineFromLocation } from '../ui/viewer-app';

const root = byId('app');
const machine = machineFromLocation();
if (machine) {
  void new ViewerApp(root, machine).boot();
} else {
  document.body.classList.remove('viewer');
  root.append(
    h(
      'main',
      { className: 'page' },
      h('h1', null, 'No machine selected'),
      h('p', null, h('a', { href: '/' }, 'Choose a machine')),
    ),
  );
}

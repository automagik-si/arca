import { createElement } from 'react';

export const name = 'arca-canvas';
export const inject = ['betterSidebar'];

// The same page supplies the tab and standalone view, including polling and CSS.
export function CanvasTab() {
  return createElement('iframe', {
    src: '/arca-canvas', title: 'Arca canvas',
    style: { display: 'block', width: '100%', height: '100%', border: 0 },
  });
}

export function apply(ctx) {
  if (!ctx.betterSidebar) return;
  ctx.effect(() => ctx.betterSidebar.registerTab({
    id: 'arca-canvas', title: 'Arca canvas', single: true, component: CanvasTab,
  }));
}

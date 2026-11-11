// Draws every Open Intelligence file in this repository from the we unfold design system's own Badge and Stamp
// components. Nothing here is drawn by hand: the script loads the design system's components/bundle.js, renders
// WeUnfold.Badge and WeUnfold.Stamp with React to static markup, and writes that markup out.
//
// Run it from a folder where react and react-dom (18.3.1) are installed:
//   node <this repo>/assets/open-intelligence/render.mjs <weunfold.ai checkout>/design-system
import { readFileSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

if (!process.argv[2]) throw new Error('usage: node render.mjs <weunfold.ai checkout>/design-system');
const require = createRequire(join(process.cwd(), 'render.cjs'));
const React = require('react');
const { renderToStaticMarkup } = require('react-dom/server');
const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, '../..');

const window = { React };
new Function('window', readFileSync(join(resolve(process.argv[2]), 'components/bundle.js'), 'utf8'))(window);
const draw = (component, props) => renderToStaticMarkup(React.createElement(window.WeUnfold[component], props));
const badge = (props) => draw('Badge', props);

// The files an <img> shows. React leaves the namespace out because it renders for a page; a file needs it.
const file = (name, markup) => writeFileSync(
  join(here, `open-intelligence-${name}.svg`),
  `${markup.replace('<svg ', '<svg xmlns="http://www.w3.org/2000/svg" ')}\n`,
);
const NIGHT = { ground: '#0e1017', color: '#ebe6da' }; // the components' own defaults, written out
const DAY = { ground: '#ffffff', color: '#11141c' }; // the light page of the components' READMEs
const INK = { ...DAY, ink: DAY.color }; // one colour
file('badge-dark', badge(NIGHT));
file('badge-light', badge(DAY));
file('badge-ink', badge(INK));
file('badge-footer', badge({ width: 170, ...DAY }));
file('seal-dark', draw('Stamp', { size: 280, ...NIGHT, subColor: '#a7abb9' }));
file('seal-light', draw('Stamp', { size: 280, ...DAY, subColor: '#4a4f5e' }));
file('seal-ink', draw('Stamp', { size: 280, ...INK, subColor: INK.color }));

// The footer badge again, on the ground and ink of the page it sits in, with the component's defaults as fallback.
// It is the cut the pages carry inline; it has no namespace and reads page tokens, so it is not a file for an <img>.
const tag = badge({ width: 170, ground: `var(--arca-bg, ${NIGHT.ground})`, color: `var(--arca-ink, ${NIGHT.color})` });
writeFileSync(join(here, 'open-intelligence-tag.svg'), `${tag}\n`);

// The pages the binary serves carry that cut inline, byte for byte.
const INLINE = /<svg [^>]*aria-label="Open Intelligence badge"[^>]*>[\s\S]*?<\/svg>/;
for (const page of ['approve_page.html', 'canvas_signin.html', 'canvas_error.html']) {
  const path = join(repo, 'crates/arca/src', page);
  const html = readFileSync(path, 'utf8');
  if (!INLINE.test(html)) throw new Error(`${page} has no Open Intelligence tag to replace`);
  writeFileSync(path, html.replace(INLINE, () => tag));
}

// The canvas builds its page with DOM calls and never innerHTML, so it takes the same markup as a tree:
// [tag, attributes, children], a child being another tree or a string of text.
const unescape = (value) => value.replace(/&(amp|lt|gt|quot|#x27);/g, (_, name) => (
  { amp: '&', lt: '<', gt: '>', quot: '"', '#x27': "'" }[name]
));
function tree(markup) {
  const root = [null, {}, []];
  const open = [root];
  const token = /<\/([\w-]+)>|<([\w-]+)((?:\s+[\w:-]+="[^"]*")*)\s*(\/?)>|([^<]+)/g;
  for (const [whole, closing, name, attributes, selfClosing, text] of markup.matchAll(token)) {
    if (closing) open.pop();
    else if (name) {
      const node = [name, {}, []];
      for (const [, key, value] of attributes.matchAll(/([\w:-]+)="([^"]*)"/g)) node[1][key] = unescape(value);
      open.at(-1)[2].push(node);
      if (!selfClosing) open.push(node);
    } else if (text !== undefined) open.at(-1)[2].push(unescape(text));
    else throw new Error(`unreadable markup: ${whole}`);
  }
  if (open.length !== 1 || root[2].length !== 1) throw new Error('the badge is not one closed element');
  return root[2][0];
}
writeFileSync(
  join(repo, 'packages/arca-canvas-dsh/src/open-intelligence.js'),
  '// Written by assets/open-intelligence/render.mjs from the we unfold Badge component. Do not edit by hand.\n'
    + `export const OPEN_INTELLIGENCE_TAG = ${JSON.stringify(tree(tag))};\n`,
);

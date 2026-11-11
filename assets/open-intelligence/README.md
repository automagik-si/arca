# Open Intelligence

The Open Intelligence tag is the slogan and stamp of [we unfold](https://weunfold.ai/), a movement for open
intelligence: AI that belongs to humanity, built in the open, by anyone, for everyone. Automagik Arca carries it.

## Where it comes from

The files here are drawn by the `Badge` and `Stamp` components of the we unfold design system: the public repository
`github.com/weunfold/weunfold.ai`, folder `design-system/`, file `components/bundle.js`, as it stood at commit
`df162c9`. That design system says of the tag: "Any project, company or person can carry it, the way a licence travels
with code."

The we unfold repository carries no licence file. The tag, the seal and the mark inside them stay we unfold's. They are
not under this repository's Apache licence, and nothing here gives a right to change them.

## The files

| File | What it is |
| --- | --- |
| `open-intelligence-badge-dark.svg` | The badge on a dark ground, the default (ground `#0e1017`, text `#ebe6da`). For READMEs, sites and decks. |
| `open-intelligence-badge-light.svg` | The badge for a light page (ground `#ffffff`, text `#11141c`). |
| `open-intelligence-badge-ink.svg` | The badge in one colour (`#11141c` on white), for print and one-ink uses. |
| `open-intelligence-badge-footer.svg` | The badge at its footer size, 170 px wide, on a light page. |
| `open-intelligence-seal-dark.svg` | The round seal on a dark ground: the mark inside two rings, OPEN INTELLIGENCE above and BUILT TOGETHER · FOR EVERYONE below. |
| `open-intelligence-seal-light.svg` | The seal on a light ground. |
| `open-intelligence-seal-ink.svg` | The seal in one colour, for rubber stamps, engraving and embroidery. |
| `open-intelligence-tag.svg` | The footer badge as the pages `arca serve` returns and the canvas carry it inline. It reads the page's `--arca-bg` and `--arca-ink`, so it is not a file for an `<img>`. |
| `render.mjs` | The script that draws all of the above from the components. |

Automagik Arca itself shows the badge in its two READMEs and the footer badge on its pages. The seals are here so that
the set is whole; nothing in this repository shows them yet.

Nothing is drawn by hand. `render.mjs` loads the design system's bundle, renders `WeUnfold.Badge` and `WeUnfold.Stamp`
with React and writes the markup out unchanged; a file for an `<img>` only gains the SVG namespace. The same run writes
the inline footer badge into `crates/arca/src/approve_page.html`, `canvas_signin.html` and `canvas_error.html`, and into
`packages/arca-canvas-dsh/src/open-intelligence.js`; tests on both sides fail when a page and that file differ.

The mark in the badge and the seal is the design system's default, Golden Circles · Fade: its small cut in the badge,
its full cut in the seal. we unfold is still choosing between three marks; when it has chosen, run the script again
against the new design system.

The words are text, as the components write them: IBM Plex Mono, weight 600 (500 for the seal's lower line), with
`monospace` as the fallback. No font is shipped here, so a reader without IBM Plex Mono sees their own monospace face.

## The rules that travel with it

These are the design system's, not ours:

- The words stay OPEN INTELLIGENCE, uppercase. Do not translate or rename them, or put a product name in the pill.
- The tag is never bigger than the mark or the wordmark beside it. In a footer it is 170 px wide; below that, use the
  mark alone.
- Dark ground by default; the light variant on light pages.
- Never redraw it by hand, stretch it, outline it, recolour it or add shadows or effects.
- The seal prints from 24 mm and reads on a screen from 84 px. Never change its words, its ring weights or the
  position of the mark.

## Drawing it again

In a folder where `react` and `react-dom` 18.3.1 are installed:

```sh
node <this repository>/assets/open-intelligence/render.mjs <weunfold.ai checkout>/design-system
```

Then rebuild the canvas bundle (`npm run build` in `packages/arca-canvas-dsh`) and run both test suites.

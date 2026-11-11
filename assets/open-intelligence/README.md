# Open Intelligence

The Open Intelligence tag is the slogan and stamp of [we unfold](https://weunfold.ai/), a movement for open
intelligence: AI that belongs to humanity, built in the open, by anyone, for everyone. Automagik Arca carries it.

Carrying it says "we support open intelligence". So the tag always holds the we unfold mark, never Automagik Arca's own
logo, and Automagik Arca uses its one-colour cut.

## Where it comes from

The files here are drawn by the `Badge` and `Stamp` components of the we unfold design system: the public repository
`github.com/weunfold/weunfold.ai`, folder `design-system/`, file `components/bundle.js`, as it stood at commit
`df162c9`. That design system says of the tag: "Any project, company or person can carry it, the way a licence travels
with code."

The we unfold repository carries no licence file. The tag, the seal and the mark inside them stay we unfold's. They are
not under this repository's Apache licence, and nothing here gives a right to change them.

## The files

| File | What it is | Used here |
| --- | --- | --- |
| `open-intelligence-badge-ink.svg` | The badge in one colour: ink `#11141c` on white. | `README.md` on a light page; the canvas package README |
| `open-intelligence-badge-ivory.svg` | The same one-colour badge for a dark ground: ivory `#ebe6da` on night `#0e1017`. | `README.md` on a dark page |
| `open-intelligence-tag.svg` | The one-colour badge at its footer size, 170 px wide, as the pages `arca serve` returns and the canvas carry it inline. It has no SVG namespace, so it is not a file for an `<img>`. It carries ivory on night as attributes; each page's stylesheet recolours the pill, the mark and the words to the page's own ink on its own ground. | the pages and the canvas |
| `open-intelligence-badge-dark.svg` | The badge with the mark in colour, on a dark ground (ground `#0e1017`, text `#ebe6da`). | no |
| `open-intelligence-badge-light.svg` | The badge with the mark in colour, for a light page (ground `#ffffff`, text `#11141c`). | no |
| `open-intelligence-badge-footer.svg` | The badge with the mark in colour at its footer size, 170 px wide, on a light page. | no |
| `open-intelligence-seal-dark.svg` | The round seal on a dark ground: the mark inside two rings, OPEN INTELLIGENCE above and BUILT TOGETHER · FOR EVERYONE below. | no |
| `open-intelligence-seal-light.svg` | The seal on a light ground. | no |
| `open-intelligence-seal-ink.svg` | The seal in one colour, for rubber stamps, engraving and embroidery. | no |
| `render.mjs` | The script that draws all of the above from the components. | |

The files marked "no" are kept so that the set we unfold publishes is whole in one place: the badges with the mark in
colour and the three seals. Nothing in this repository shows them. Use the one-colour badge unless we unfold's rules
call for another.

The design system's kit shows the one-colour badge once, as ink on white. The ivory file is the same component drawn
with the same one-colour setting (`ink`), given ivory instead of ink and the component's own dark ground; the design
system names ivory as the one colour for dark grounds.

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

These are the design system's own words, copied from it, not ours.

From its `README.md`:

> - Open Intelligence is never bigger than the mark or the wordmark.
> - Never add yellow to the mark (gold is a UI accent, outside the mark), recolour single seeds, rings or arms, or
>   reorder the colours.
> - Never redraw by hand, stretch, outline, add shadows or effects. Anything that cannot be traced back to the
>   formulas is not part of the mark.

From `components/Badge/README.md`:

> - Provide `width` (default 340 px; 170 px in a footer). Below 170 px use the mark alone.
> - Dark ground by default; pass `ground: '#ffffff'` and `color: '#11141c'` on light pages, and `ink` for a
>   one-colour badge.
> - The words stay OPEN INTELLIGENCE, uppercase; do not translate, rename or add a product name inside the pill.

From `components/Stamp/README.md`:

> - Provide `size`; it prints from 24 mm and reads on screen from 84 px (pinned to an avatar).
> - Never change the words, the ring weights or the position of the mark.

## Drawing it again

`render.mjs` is a maintainer's tool. The build and the tests never run it. Running it executes we unfold's
`components/bundle.js` as ordinary JavaScript, with every right your Node process has, and reads a checkout that lives
outside this repository. Read the bundle you point it at before you run it.

In a folder where `react` and `react-dom` 18.3.1 are installed:

```sh
node <this repository>/assets/open-intelligence/render.mjs <weunfold.ai checkout>/design-system
```

Then rebuild the canvas bundle (`npm run build` in `packages/arca-canvas-dsh`) and run both test suites.

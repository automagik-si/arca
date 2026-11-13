# Logo

The triskle is Automagik's mark, and Automagik's alone. It is the owner's clay render with one clear line along the seams. Genie, Omni and Mykes each have their own flat mark: a lamp, a tentacle vortex and the cords (the README's rules 1, 9 and 10).

## The mark: `svg/` and `png/`

- **`triskle-three-colours` is the mark.** `triskle-ink` (for light grounds) and `triskle-cream` (for dark grounds) are the fallback where colour cannot be used.
- `svg/triskle-<colour>.svg`: the large drawing, from about 100px up. `-64`, `-32`, `-16`: the drawings for those sizes, with a wider line.
- `png/triskle-<colour>-1024.png` and `-512.png`: clear ground. Other sizes: `../export-png.mjs`.
- All the triskle's files share one square frame, so they line up when swapped.
- These are pictures, not single-ink shapes: CSS `color` does not change them. Pick the colour by file.

## Genie's, Omni's and Mykes's marks: `svg/`

Flat and one colour. The owner approved the lamp and the vortex on 12 November 2026 and confirmed it on 13 November 2026, and picked the redrawn Mykes drawing on 13 November 2026. None carries the triskle.

- `genie-lamp-ink.svg`, `-cream.svg`: Genie's mark, a flat lamp. One drawing.
- `omni-mark-ink.svg`, `-cream.svg`: Omni's mark, a tentacle vortex: eight tapered arms from the rim, their tips curling round the open centre, with suckers as holes. For about 56px and up.
- `omni-mark-ink-32.svg`, `-cream-32.svg`: the same vortex drawn for small sizes, about 32px: no suckers, thicker tips.
- `mykes-clean-ink.svg`, `-cream.svg`: Mykes's mark, the cords: threads that run out from one knot, redrawn flat on clean arcs with round ends. One drawing, in a frame 219.93 wide: the redrawn one the owner picked, not the other drawing of the cords ("clean forks", 219.77 wide), which is not here.
- Each file is one filled shape. `-ink` is `#3c3049`, for light grounds; `-cream` is `#f3e9dd`, for dark grounds. The colour is in the file: CSS `color` does not change it. Pick the colour by file.
- No masks and no blur: any tool draws them.
- Each mark has its own frame, cut to its ink: the lamp is wide (392 by 200), the vortex is square, the cords are 220 by 222. They do not share the triskle's square frame.
- None is drawn for 16px (the README's "Which logo file").

## Lockups: `lockups/`

The mark with the wordmark: the triskle for `automagik`, and each product's own mark for `genie`, `omni` and `mykes`.

- `genie-lamp-lockup-ink.svg`, `-cream.svg`: the lamp with the genie word.
- `omni-mark-lockup-ink.svg`, `-cream.svg`: the vortex with the omni word, for about 56px high and up.
- `omni-mark-lockup-ink-header.svg`, `-cream-header.svg`: the same with the vortex's small drawing, for a header and anything under about 56px high.
- `mykes-clean-lockup-ink.svg`, `-cream.svg`: the cords with the mykes word.
- They share Automagik's frame: 264 units high, the small letters 136 high on a baseline at 18, and the same space from the mark to the word. At one height the letters are the same size in all of them.
- The vortex and the cords fill the box Automagik's triskle fills. The lamp is wide and short, so it is centred on the small letters, and its foot sits a little under their baseline.
- The genie and omni words are the files in `wordmark/`, placed whole and unchanged, with their sparkle. The mykes word has no i and so no sparkle; its letters are the same outline as the mykes files in `wordmark/`.

**The `genie-lockup-*` and `omni-lockup-*` files are RETIRED.** They carry the triskle, which is Automagik's alone. Do not use them: Genie's lockup is `genie-lamp-lockup-*` and Omni's is `omni-mark-lockup-*`. The files stay here for older work that points at them, and they are not to be changed.

The notes below are for Automagik's lockup; the retired files were built the same way, in the same frame.

- **`automagik-lockup-three-colours-ink` and `-three-colours-cream` are the lockups.** Ink letters for light grounds, cream letters for dark grounds.
- `-header`: the same with the wider seam line, for a lockup under about 56px high.
- `automagik-lockup-ink` and `-cream`: the one-colour fallback.
- The dot on the i is a sparkle in the product's colour: Automagik's is violet, Genie's is pink and Omni's is cyan. It is 60 units across where the small letters are 100 high.

## Wordmark: `wordmark/`

The name alone, in the approved round lowercase lettering, with the product's sparkle on the i. The letters are outlines; no font is needed. A product's word alone is a correct form, as Automagik's name without its mark is (the README's rule 10). The mykes word has no i and so no sparkle.

- One file per name and letter colour: `-ink` for light grounds, `-cream` for dark grounds.
- The files are as high as the lockup frame, so a wordmark and a lockup at one height have letters of one size.
- **This is the one place the lettering is drawn.** The lockups are built from these files. To change the wordmark, replace these files under the same names and rebuild the lockups. (The mykes lockup is the one exception: it is we unfold's file as handed over, and its letters are the same outline as the mykes wordmark file's.)
- `mykes-wordmark-ink.svg` and `mykes-wordmark-cream.svg`: the word "mykes" in the same lettering, for the family's menu drawer, where Omni, Genie and Mykes each stand as a word with no mark. They are we unfold's files, the ones the Mykes deck uses, kept here as byte copies (there named `mykes-word-day.svg` and `mykes-word-night.svg`). The word has no sparkle. Its frame is 257 units high where the others' is 264, on the same line: show it at 257/264 of the others' height and the letters are one size.

## Where Genie's, Omni's and Mykes's files came from

They are we unfold's drawings, from its logo sheets `logo-family-05` (the lamp, the cords) and `logo-family-06` (the vortex, the lockups), handed over on 13 November 2026 as the package `family-marks`. Its `SHA256SUMS` file has the sha256 `14302472abf273b6767687946b86286185d99bf6b229792c4b13fa055d13d831`, and Genie's and Mykes's files here were checked against it. The package's `mykes-clean` files are the redrawn cords (its notes: "Cords as first drawn, redrawn flat"), the drawing the owner picked.

Omni's eight files are from a second package, `omni-marks-light`, handed over the same day: the same drawing on curves (the next section). Its `SHA256SUMS` file has the sha256 `57f9f245857ca4aa588959d36137275b219729368585ea5d6dbbc4527d68fc49`, and Omni's files here were checked against it.

Each file here is a byte copy of its package's file. Only the name changed, to this folder's naming: `-day` is `-ink`, `-night` is `-cream`, the size comes last, the small lockup is `-header`, and a lockup carries its mark's name so that it is not taken for a retired file.

From `family-marks`:

| File | In the package | sha256 |
| --- | --- | --- |
| `svg/genie-lamp-ink.svg` | `marks/genie-lamp-ink.svg` | `a837a92a18fb4a319ce17b1d91e0f3f347c2feb3e8c49aaa696ea1e5154b6914` |
| `svg/genie-lamp-cream.svg` | `marks/genie-lamp-cream.svg` | `ef94842939a9219c21d1c75b6570605500e9dcb03ec133b3e9a2b527d5514724` |
| `svg/mykes-clean-ink.svg` | `marks/mykes-clean-ink.svg` | `464d74f76cfe6ab5ad351d07938736bd68697a3210e0a82cfbe580d068c292ba` |
| `svg/mykes-clean-cream.svg` | `marks/mykes-clean-cream.svg` | `74cdf63bb480c803d4ea7e63677e78594890fbe598294d997f50a0077780ffd9` |
| `lockups/genie-lamp-lockup-ink.svg` | `lockups/genie-lockup-day.svg` | `5efc6f5a6a109638a98dd69730e67361c4617f3ccdd60d79546d6712abbb9335` |
| `lockups/genie-lamp-lockup-cream.svg` | `lockups/genie-lockup-night.svg` | `3bce670e43c4293f5e2617b8f56aeebd820448a66ba88cec7686c7e4f226cc60` |
| `lockups/mykes-clean-lockup-ink.svg` | `lockups/mykes-clean-lockup-day.svg` | `d5f4c9d4fbdd1b36b1baf3e9d94b43d87a15468e4ee0e669072e06048c5ee6d1` |
| `lockups/mykes-clean-lockup-cream.svg` | `lockups/mykes-clean-lockup-night.svg` | `3a36b5cbea56c95eec22a01f1166b3bce4adcaad94eb6fd4b5f4e04f38746758` |

From `omni-marks-light`:

| File | In the package | sha256 |
| --- | --- | --- |
| `svg/omni-mark-ink.svg` | `omni-mark-ink.svg` | `9b913491327001c9e3e65d38b3bda614b412da14eaa2ed2449eb0c9ac50b953a` |
| `svg/omni-mark-cream.svg` | `omni-mark-cream.svg` | `cedaf4516d1731ddc338a67df075aa7c5fa595d7dbddc017a72617866f92c29e` |
| `svg/omni-mark-ink-32.svg` | `omni-mark-32-ink.svg` | `9de3588852dc64ae54783e63fbb53de22b83626ad47139c17655a1e943cb9e2e` |
| `svg/omni-mark-cream-32.svg` | `omni-mark-32-cream.svg` | `6fb43b5a6fc57c4365bbfc37b696def28fab5cdd8b739d97907621ef5293a5d9` |
| `lockups/omni-mark-lockup-ink.svg` | `lockups/omni-lockup-day.svg` | `820d20dd9efdab808843829633167056d7c67c4c764f89909f93ae61ff42ea3b` |
| `lockups/omni-mark-lockup-cream.svg` | `lockups/omni-lockup-night.svg` | `e7132f429a0c94544fe67c5c7450751807a25c6bc9564aa064d8bdeec442eb17` |
| `lockups/omni-mark-lockup-ink-header.svg` | `lockups/omni-lockup-small-day.svg` | `cef19f7a34cba707c8995eac9ded832bc6b4ddf6361793ed62ddc6d7d20c387a` |
| `lockups/omni-mark-lockup-cream-header.svg` | `lockups/omni-lockup-small-night.svg` | `8af9ca4d1c5b075c98913b82ae63ef9b9a9b28e0babea28d6e71f0f95fe08f16` |

A file whose sha256 is not the one in these tables was changed: put its package's file back (`scripts/brand-marks.test.ts` checks it).

## Omni's vortex on curves

Omni's files are the same drawing as the one first handed over, with its outline written on curves. The first files traced the vortex with about 4,600 short straight pieces, 55 to 61 KB a file. The files here draw each arm with 12 or 13 curves, its round tip as two arcs and the ten facets of its end at the rim as before: 6.7 KB for the mark, 4.3 KB for its small drawing, 8.5 and 6.1 KB for the lockups.

- It is not a redrawing (the README's rule 8). Only the outline's path data changed: the frame, the label, the colour, the even-odd holes and the single shape are the first files' bytes, and so are the lockups' placing, word and sparkle.
- The 40 suckers of the large drawing keep their numbers. Both drawings keep their count of shapes: 8 arms and 40 suckers, and 8 arms.
- Laid over the first files, drawn large and brought down to 128px, the two cover the same area to 0.9996 (the large drawing) and 0.9977 (the small drawing), as intersection over union. On the 120-unit frame the curves stay within 0.0145 of the first outline. The tips, which were many-sided and are now true arcs, stay within 0.042: the one place the outline is smoother than before.
- Old and new were looked at side by side on 13 November 2026, on both grounds: the marks at 128, 32 and 16px, the lockups at 60 and 28px high. They look the same; the only differences are in how the edge pixels are shaded.
- At 16px the vortex still closes into a blot, as before: the curves change nothing in the sizes each drawing is for.

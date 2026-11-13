---
name: automagik-brand
description: The Automagik brand (Automagik, Genie, Omni) as files in this repository's brand folder (docs/brand, unless SOURCE.json beside this skill names another). Use before any work on a logo or mark, a colour, type or fonts, a UI surface (a page, a screen, a form, a component, a sidebar, any HTML or CSS a person will see), a web or landing page, a slide deck, or brand art. Use it even when the request does not name the brand.
metadata:
  internal: true
---

# Automagik brand

The brand is a folder in this repository. It is the only source. Read the files; do not work from a
memory of how the brand looks, and do not look for it anywhere else.

## Where the folder is

- `docs/brand/`, from the repository's root: in Automagik's repository, and in a copy unless the
  next line applies.
- When a file named `SOURCE.json` is beside this file, read it. Its `into` names the folder, from
  the repository's root (`brand`, for example).

Every path below is inside that folder.

## Before you draw, style or write markup

1. Read `README.md`, all of it. It holds the rules, the file tables and the notes on colour, type,
   shape and components. Everything below points into it and adds nothing to it.
2. For a page, a screen or a component, use `tokens/tokens.css` as it is: link it, or copy its
   declarations without changing a value. `tokens/tokens.json` is the record of the same values,
   with a note on each (README, "Tokens"). Then look in `components/` for the component before you
   make one (README, "Components").
3. For a logo, pick the file from the README's "Which logo file" table and use it unchanged.

## What the README rules out

Each line names where the README says it. When this file and the README differ, the README is right.

- The triskle is Automagik's mark alone. Genie, Omni and Mykes do not carry it (rule 1).
- The `genie-lockup-*` and `omni-lockup-*` files are RETIRED: they carry the triskle. Never use
  them (rule 1, "Which logo file").
- Genie, Omni and Mykes each have their own mark: a flat lamp, a tentacle vortex, the cords.
  Take the mark from `logo/svg/` or its lockup from `logo/lockups/` (`genie-lamp-lockup-*`,
  `omni-mark-lockup-*`, `mykes-clean-lockup-*`), by the "Which logo file" table. The word alone is
  still a correct form. Under Mykes: "Coming soon." in plain words (rules 9 and 10).
- Beside Genie's, Omni's or Mykes's word goes its own mark from this folder and nothing else: never
  the triskle, a pet or another drawing. Each of these marks is flat and one colour, with its
  word's sparkle as the only colour (rule 9).
- None of the three marks is drawn for 16px: no favicon or 16px icon from them. Under about 56px
  Omni takes its small drawing (`-32`, and the lockup's `-header`) ("Which logo file").
- Automagik's triskle is always in colour. The ink and cream marks are only for places where
  colour cannot be used (rule 2).
- Pets are never marks. Genie and Omni are characters: never in a header, a favicon or an app icon in
  the logo's place, and never beside a product's word (rules 3 and 10).
- The only Genie is the full-body one, from `genie/`. Never crop to the head (rule 4, "Genie").
- Pick the drawing made for the size you show. Never scale the large drawing down to an icon
  (rule 6).
- The dot on the i is one sparkle, one colour per product: Automagik violet, Genie pink, Omni cyan.
  It is in the wordmark and lockup files already; do not draw it (rule 7).
- Never redraw a mark, the letters or Genie: no moving, reshaping, slimming, recolouring or
  outlining. Use the files (rule 8).
- Never invent a colour. Every colour comes from a token, and `triskle-cyan`, `triskle-pink` and
  `triskle-violet` are never text and never a UI colour ("Colour").
- A status dot never stands alone: every one has a word or a mark beside it. The status colours
  are `st-ok`, `st-bad`, `st-warn` and `st-work`, with `st-okdot` and `st-warndot` for dots
  ("Colour").
- Type is two families: Fredoka for titles, labels and buttons, and DM Sans for running text, used
  as "Type" says. The logo's letters are a drawing, never set in a font.

## When the folder does not have it

The README's "Not here" lists what this folder leaves out on purpose, and for some of it says where
it is: motion, the stage gradient and the tile colours are in the component stylesheet. Follow that
list. Use the place it names, and do not treat a thing it leaves out as part of this set.

## In a repository that carries a copy

If `SOURCE.json` is beside this file, the folder is a copy of Automagik's (so is one with a
`SOURCE.json` inside `docs/brand/`, made by the copy tool before the folder could be chosen). Do not
edit it here (README, "Using this in another repository").

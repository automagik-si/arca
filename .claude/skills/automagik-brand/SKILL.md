---
name: automagik-brand
description: The Automagik brand (Automagik, Genie, Omni) as files in docs/brand. Use before any work on a logo or mark, a colour, type or fonts, a UI surface (a page, a screen, a form, a component, a sidebar, any HTML or CSS a person will see), a web or landing page, a slide deck, or brand art. Use it even when the request does not name the brand.
---

# Automagik brand

The brand is a folder in this repository: `docs/brand/`. It is the only source. Read the files; do not
work from a memory of how the brand looks, and do not look for it anywhere else.

## Before you draw, style or write markup

1. Read `docs/brand/README.md`, all of it. It holds the rules, the file tables and the notes on
   colour, type, shape and components. Everything below points into it and adds nothing to it.
2. For a page, a screen or a component, use `docs/brand/tokens/tokens.css` as it is: link it, or copy
   its declarations without changing a value. `docs/brand/tokens/tokens.json` is the record of the
   same values, with a note on each (README, "Tokens"). Then look in `docs/brand/components/` for the
   component before you make one (README, "Components").
3. For a logo, pick the file from the README's "Which logo file" table and use it unchanged.

## What the README rules out

Each line names where the README says it. When this file and the README differ, the README is right.

- The mark is always the triskle, for Automagik, Genie and Omni. There is no other logo (rule 1).
- The triskle is always in colour. The ink and cream marks are only for places where colour cannot
  be used (rule 2).
- Pets are never marks. Genie and Omni are characters: never in a header, a favicon or an app icon in
  the logo's place (rule 3).
- The only Genie is the full-body one, from `docs/brand/genie/`. Never crop to the head (rule 4,
  "Genie").
- Pick the drawing made for the size you show. Never scale the large drawing down to an icon
  (rule 6).
- The dot on the i is one sparkle, one colour per product: Automagik violet, Genie pink, Omni cyan.
  It is in the wordmark and lockup files already; do not draw it (rule 7).
- Never redraw a mark, the letters or Genie: no moving, reshaping, slimming, recolouring or
  outlining. Use the files (rule 8).
- Never invent a colour. Every colour comes from a token, and `triskle-cyan`, `triskle-pink` and
  `triskle-violet` are never text and never a UI colour ("Colour").
- Type is two families, Manrope and DM Sans, used as "Type" says.

## When the folder does not have it

The README's "Not here" lists what this folder leaves out on purpose, and for some of it says where
it is: motion, the stage gradient and the tile colours are in the component stylesheet. Follow that
list. Use the place it names, and do not treat a thing it leaves out as part of this set.

## In a repository that carries a copy

If `docs/brand/SOURCE.json` exists, this folder is a copy of Automagik's. Do not edit it here
(README, "Using this in another repository").

# Vendored Font Awesome

Font Awesome Free 6.4.0 by @fontawesome — https://fontawesome.com

- `css/all.min.css` — Code: MIT License
- `webfonts/fa-solid-900.woff2` — Icons: CC BY 4.0, Fonts: SIL OFL 1.1
  (Copyright 2023 Fonticons, Inc.)

Only the `fa-solid-900` webfont is vendored: every icon in the
collaboration room uses the solid (`fas`) style. The minified CSS also
declares `fa-regular-400`, `fa-brands-400`, and `fa-v4compatibility`
`@font-face` rules; those fonts are fetched only if an element uses the
`far`/`fab` classes, which the room does not. Their missing `.woff2`/
`.ttf` files therefore never load.

If a new room icon uses a non-solid family, vendor the matching
webfont here and re-check `docs/port-status.md` (G6).

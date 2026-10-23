# Assets

Served from the binary (`include_bytes!` in `src/assets.rs`); nothing is fetched from a CDN.

| File | Source | SHA-256 |
|---|---|---|
| `datastar.js` | [starfederation/datastar](https://github.com/starfederation/datastar) `v1.0.4` (commit `1efcdc3cb336ec3e9139491604e770e0329657bc`), `bundles/datastar.js` | `727844adfc825ee651fb93c544a2a739986f9a21820a94524b35f0cac470cf91` |
| `datastar.LICENSE.md` | the same commit's `LICENSE.md` (MIT) | `17da0e216d3c6a819526f5b72a6a44d84d34bcd9beb4191ac6af89c1508ef08f` |
| `fonts/libre-franklin-latin-wght-normal.woff2` | npm `@fontsource-variable/libre-franklin` `5.3.0`, `files/` | `52e605852c72cc2df1473df1fb7f26a3a0be58e31835bc8f33f29b1918c82cbd` |
| `fonts/OFL.txt` | the same package's `LICENSE` (SIL Open Font License 1.1) | |
| `mark.svg` | `site/public/favicon.svg` | |
| `manifest.webmanifest` | written here: the web app manifest that makes the dashboard installable (ADR 0042) | |
| `icon-192.png`, `icon-512.png` | `mark.svg`: `rsvg-convert -w 192 -h 192 mark.svg -o icon-192.png`, and 512 | |
| `icon-maskable-512.png` | `mark.svg` with its bars wrapped in `<g transform="translate(16 16) scale(0.78) translate(-16 -16)">`, inside the maskable safe zone, rendered at 512 the same way | |

The Rust SDK `datastar` 0.4.1 supports Datastar 1.0.4. To update the bundle, change the file and its row here
together: `sha256sum -c` against this table before committing.

Each file is served at a URL versioned by a digest of its bytes (`assets::path`), cached for good. `style.css` names
the font by that URL, so a new font file needs its `?v=` there too; the assets test prints the value. The manifest
names its icons without `?v=`, so they are revalidated by their `ETag` each time; a new icon needs nothing else.

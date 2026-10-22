# Vendored assets

Served from the binary (`include_bytes!` in `src/assets.rs`); nothing is fetched from a CDN.

| File | Source | SHA-256 |
|---|---|---|
| `datastar.js` | [starfederation/datastar](https://github.com/starfederation/datastar) `v1.0.4` (commit `1efcdc3cb336ec3e9139491604e770e0329657bc`), `bundles/datastar.js` | `727844adfc825ee651fb93c544a2a739986f9a21820a94524b35f0cac470cf91` |
| `fonts/libre-franklin-latin-wght-normal.woff2` | npm `@fontsource-variable/libre-franklin` `5.3.0`, `files/` | `52e605852c72cc2df1473df1fb7f26a3a0be58e31835bc8f33f29b1918c82cbd` |
| `fonts/OFL.txt` | the same package's `LICENSE` (SIL Open Font License 1.1) | |
| `mark.svg` | `site/public/favicon.svg` | |

The Rust SDK `datastar` 0.4.1 supports Datastar 1.0.4. To update the bundle, change the file and its row here
together: `sha256sum -c` against this table before committing.

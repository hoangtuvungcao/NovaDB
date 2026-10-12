# NovaDB brand guide

## Logo files

| File | Use |
|---|---|
| `logo-mark.svg` | Icon only (square, 64x64 grid). App icons, favicons, avatars. Readable at 16 px. |
| `logo.svg` | Icon + wordmark. The wordmark switches colors with `prefers-color-scheme` when the SVG is loaded as an image. |
| `logo-light.svg` | Icon + wordmark for **light** backgrounds (dark text). |
| `logo-dark.svg` | Icon + wordmark for **dark** backgrounds (white text). |
| `banner.svg` | 1280x400 README / social hero. |
| `../novadb_logo.jpg`, `../novadb_banner.jpg` | Legacy raster artwork, kept for old links. Do not use in new material. |

The mark combines a database cylinder (the storage engine) with a four-point "nova" star (local-first sync and the
spark of a new project) on a violet tile.

The wordmark is set in live text (`Inter`, falling back to the system sans-serif). If you need pixel-identical output
(print, merchandise), convert the text to outlines in a vector editor with Inter Bold installed.

## Palette

| Role | Name | Hex | Notes |
|---|---|---|---|
| Primary | Nova Violet | `#5B4BF5` | Links, buttons, "DB" in the wordmark on light backgrounds |
| Primary (light tint) | Lavender | `#A99FFF` | "DB" in the wordmark on dark backgrounds, dark-mode links |
| Primary gradient | Violet to Indigo | `#7C6CFF` to `#3B2FD9` | Icon tile only (135 degrees) |
| Secondary | Signal Cyan | `#22D3EE` | Highlights, prompts, charts, secondary accents |
| Accent | Nova Amber | `#FFC857` | The star. Use sparingly: one accent per view |
| Ink | Deep Space | `#0B1026` | Text on light backgrounds, dark background base |
| Dark surface | Night | `#161046` | Gradient end / cards on dark backgrounds |
| Light background | Paper | `#FFFFFF` / `#F6F5FF` | Pages and cards on light themes |
| Muted text (dark) | Mist | `#C9CCE8` | Body text on dark backgrounds |
| Muted text (light) | Slate | `#4B5170` | Secondary text on light backgrounds |

Contrast: `#5B4BF5` on white is 5.6:1 and `#A99FFF` on `#0B1026` is 8.1:1, both meeting WCAG AA for body text.
Do not put amber text on white (insufficient contrast).

## Typography

- **Headings and UI:** Inter (700/800 for headings, 400/500 for body). Fallback: `'Segoe UI', 'Helvetica Neue', Helvetica, Arial, sans-serif`.
- **Code:** JetBrains Mono. Fallback: `'SFMono-Regular', Menlo, Consolas, monospace`.
- Wordmark: Inter Bold, tracking -1 at 36 px. "Nova" in ink/white, "DB" in the primary color.

## Usage rules

1. Keep clear space around the logo equal to the height of the star (about one quarter of the icon height).
2. Minimum size: icon 16 px; full logo 96 px wide.
3. Do not recolor, rotate, stretch, outline, or add shadows to the mark. Do not place the full-color mark on busy photos.
4. On dark backgrounds use `logo-dark.svg`; on light backgrounds use `logo-light.svg`. Use `logo.svg` where the
   viewer's color scheme is unknown (e.g. GitHub README).
5. Write the name as **NovaDB** (one word, capital N, D, B). The CLI and packages are lowercase `novadb`.
6. The brand belongs to the NovaDB project. Third parties may use the logo to refer to NovaDB (e.g. "works with
   NovaDB") but not in a way that implies endorsement.

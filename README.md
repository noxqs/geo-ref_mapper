# geo-ref_mapper

A fast desktop image viewer for Linux that can also place scanned historical
maps on the earth.

It has two uses, and the second can be switched off:

1. **Browsing pictures.** Index folders of photos and images, browse them as a
   tree or a thumbnail mosaic, read their metadata, and see where a photo was
   taken.
2. **Working with old maps.** Turn large map scans into files that zoom
   quickly, pin them to a modern map by hand, and fade between maps from
   different years to compare how a place changed.

## Pages

The pill in the title bar switches between pages:

| Page | What it is for |
|---|---|
| **Viewer** | One picture at a time, with pan and zoom. Gigapixel tiled TIFFs load only the part on screen. |
| **Mosaic** | Thumbnail grid of the current folder. Related pictures can be grouped into stacks. |
| **Map** | An OpenStreetMap basemap with placed maps drawn over it, and a timeline strip that fades maps in by year. |
| **Map Images** | The queue of maps being converted and placed. |
| **Formats** | Which image formats this build can open. |
| **Export** | Resize, crop and convert many files at once. Originals are never changed. |
| **Config** | Appearance, interface scale, map tiles, install, and default-viewer settings. |

The FILES panel (left) holds the folder tree and pinned folders. The INFO panel
(right) shows EXIF, XMP and other metadata, the photo's location, and map
details.

## Pictures

- **Formats:** PNG, JPEG, GIF, WebP, BMP, ICO, TIFF (including BigTIFF and
  Cloud-Optimized GeoTIFF), OpenEXR, Radiance HDR, Photoshop, HEIC/HEIF,
  JPEG XL, QOI, TGA, DDS, Netpbm, farbfeld, SVG and GIMP. AVIF, camera RAW and
  JPEG 2000 are not supported yet.
- **Library:** folders are indexed into a SQLite database, with thumbnails
  stored inside it. Scanning and thumbnailing run in the background.
- **Location:** a photo's GPS position is turned into a country and nearest town
  using data built into the program, with no network use. A street address is
  looked up online only when you press the button, and only for that one photo.
- **Organising:** mark pictures, then move or copy them between folders. Nothing
  is ever overwritten.
- **Speed:** decoding happens on worker threads, JPEGs with restart markers are
  decoded in parallel, and the app picks a graphics card that has memory free,
  falling back to the integrated GPU or software rendering.

## Maps

1. **Convert.** A large scan is rewritten as a tiled, layered TIFF (full size,
   half, quarter, and so on) so it opens and zooms quickly. Information from a
   `<picture>.histomap.json` sidecar beside the source (title, date, source,
   rights, crop) is written into the new file.
2. **Place.** Mark the same point (a crossroads, a river mouth) on the basemap
   and on the scan. With enough pairs the program works out where the map sits
   and shows how far off each point is. The answer is written into the TIFF as
   standard GeoTIFF tags, so other GIS software can read it too.
3. **Compare.** On the Map page, scroll the timeline strip to fade between maps
   of the same area from different years.

Map support can be turned off under Config → **IMAGE MAPS**, which leaves a plain
image viewer.

## Keys

| Key | Action |
|---|---|
| Down / Up | Next / previous picture |
| Right / Left | Open / close the folder in the tree |
| F or 0 | Fit to window |
| F11 | Full screen |
| Ctrl+O | Open a file |
| Space | Viewer: mark the picture. Elsewhere: put it on the map |
| D | Show the tile grid of a layered image |
| G | Map: show a coordinate grid |
| L | Map: lock or unlock anchor points |

## Install

Download the Linux x86_64 archive from
[Releases](https://github.com/noxqs/geo-ref_mapper/releases), unpack it and run
`geo-ref_mapper`. Config → **INSTALL** copies it into `~/.local` (no root
needed) so it appears in the application menu. The same page can make it the
default viewer for image files.

Open a file directly with:

```sh
geo-ref_mapper photo.jpg
```

The library is kept in the user data directory
(`~/.local/share/geo-ref_mapper/library.db` on most Linux systems).

If the graphics card is out of memory, choose another renderer:

```sh
GEO_REF_MAPPER_RENDERER=integrated geo-ref_mapper
GEO_REF_MAPPER_RENDERER=software geo-ref_mapper
```

## Build from source

Needs a recent stable Rust toolchain (edition 2024) and, on Debian or Ubuntu:

```sh
sudo apt-get install pkg-config nasm libx11-dev libxkbcommon-dev libwayland-dev
cargo build --release
cargo test --release
```

Pushing a `v*` tag builds a release and attaches it on GitHub
(`.github/workflows/release.yml`).

## Notes

`NOTES.md` is the working log: what changed each session, why, and what is still
open.

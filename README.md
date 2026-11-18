# JPVinylRec

Desktop app (Rust + egui) that turns a recording of one vinyl side into speed-corrected, split and tagged FLAC tracks.

1. **Release** – paste a Discogs release URL/ID or a MusicBrainz release URL. Pick the side.
2. **Side recording** – open the FLAC of that side (one file per side, 24-bit, peaks around −6 dBFS).
   Do any declicking (Audacity Click Removal, ClickRepair, RX) *before* this step.
3. **Speed** – choose how to correct the turntable speed:
   - **Reference** (best): add a CD rip or download of one or more tracks from the side (any format/sample rate).
     The app finds it in the recording and aligns their energy envelopes at dozens of anchor points.
     Accurate to about ±0.001 %.
   - **Database side total**: music length of the side ÷ sum of the database track lengths.
     Good to roughly ±0.3–0.6 % depending on how the database lengths were measured.
   - **Manual**, or **none**.
4. **Splits** – boundaries are placed in the silent gaps nearest to where the database lengths say the
   tracks change (lengths are scaled to the recording, so the speed error doesn't matter here).
   Yellow = in a gap, red = no gap found (segue) – check it, purple = moved by you.
   Drag markers, nudge with arrow keys, use ▶ start / ▶ end to listen (preview is speed-corrected).
5. **Export** – each track is resampled (pitch and tempo together, like the platter speed) with a
   high-order windowed-sinc interpolator and written as FLAC to
   `<library>/<Album Artist>/<Album> (<original year>)/NN - Title.flac`, with tags
   (title, artists, album, track numbers, original year, label, catalogue number, Discogs/MusicBrainz IDs)
   and embedded cover art, plus `cover.jpg` in the folder for Plex.

Speed convention in the UI: "turntable 99.137 %" means the platter ran 0.863 % slow.

## Download

Every push to `main` is built by GitHub Actions (Actions tab -> latest run -> *jpvinylrec-windows* artifact).
Pushing a tag like `v0.1.0` also publishes `jpvinylrec.exe` on the Releases page.

## Build (Windows)

```
rustup update
cargo build --release
target\release\jpvinylrec.exe
```

You can also start it with a recording: `jpvinylrec.exe "D:\Rips\side-a.flac"` (or use *Open with*).

The Discogs API works without a token, but cover images and higher rate limits need a personal token
(Settings in the left panel; get one at discogs.com/settings/developers).

## Tests

`cargo test --release` builds a synthetic side with a 0.87 % speed error, lead-in crackle, clicks
and run-out groove, then checks speed recovery (reference and database), split placement,
tagged FLAC output and the residual speed error after correction.

`cargo run --release --example make_demo -- <folder>` writes that demo side and a CD-style reference
of track 2, so you can try the app without a turntable.

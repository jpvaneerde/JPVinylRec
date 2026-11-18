#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use jpvinylrec::analysis::{analyse, Analysis, HOP};
use jpvinylrec::audio::{self, Audio};
use jpvinylrec::export::{self, Cut, Report, Settings};
use jpvinylrec::meta::{self, Release};
use jpvinylrec::resample::{resample, Kernel};
use jpvinylrec::speed::{self, Estimate, RecFeatures};
use jpvinylrec::split::{self, fmt_time, Marker, MarkerKind};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

const DEFAULT_ROOT: &str = r"\\192.168.1.138\Media\Music\Audio";

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Prefs {
    output_root: String,
    discogs_token: String,
    /// 0 = same as recording
    bits: u32,
    embed_cover: bool,
    overwrite: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { output_root: DEFAULT_ROOT.into(), discogs_token: String::new(), bits: 0, embed_cover: true, overwrite: false }
    }
}

enum Msg {
    Release(Result<Release, String>),
    Cover(Option<Vec<u8>>),
    Recording(Result<(PathBuf, Arc<Audio>, Analysis, Arc<RecFeatures>), String>),
    Reference(PathBuf, Result<Estimate, String>),
    Progress(f32, String),
    Exported(Result<Report, String>),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Method {
    Reference,
    Database,
    Manual,
    Off,
}

struct App {
    prefs: Prefs,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,

    release_input: String,
    release: Option<Release>,
    cover: Option<Vec<u8>>,
    side: String,
    fetching: bool,

    rec_path: Option<PathBuf>,
    audio: Option<Arc<Audio>>,
    analysis: Option<Analysis>,
    recf: Option<Arc<RecFeatures>>,
    loading: bool,

    refs: Vec<(PathBuf, Option<Result<Estimate, String>>)>,
    method: Method,
    manual_speed: f64,

    markers: Vec<Marker>,
    view: (f64, f64),
    drag: Option<usize>,
    selected: Option<usize>,

    exporting: Option<(f32, String)>,
    report: Option<Result<(PathBuf, usize, usize), String>>,
    error: Option<String>,

    stream: Option<(rodio::OutputStream, rodio::OutputStreamHandle)>,
    sink: Option<rodio::Sink>,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        let prefs = cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let (tx, rx) = channel();
        let mut app = App {
            prefs,
            tx,
            rx,
            release_input: String::new(),
            release: None,
            cover: None,
            side: String::new(),
            fetching: false,
            rec_path: None,
            audio: None,
            analysis: None,
            recf: None,
            loading: false,
            refs: Vec::new(),
            method: Method::Off,
            manual_speed: 100.0,
            markers: Vec::new(),
            view: (0.0, 1.0),
            drag: None,
            selected: None,
            exporting: None,
            report: None,
            error: None,
            stream: None,
            sink: None,
        };
        // `jpvinylrec.exe side-a.flac` (or "Open with") loads that recording straight away.
        if let Some(p) = std::env::args_os().nth(1).map(PathBuf::from).filter(|p| p.is_file()) {
            app.load_recording(p, &cc.egui_ctx);
        }
        // Debug builds only: load a release from a saved Discogs JSON file (offline testing).
        #[cfg(debug_assertions)]
        if let Ok(p) = std::env::var("NEEDLEDROP_DISCOGS_JSON") {
            if let Ok(v) = std::fs::read_to_string(&p).map(|s| serde_json::from_str(&s)) {
                if let Ok(v) = v {
                    let _ = app.tx.send(Msg::Release(Ok(meta::parse_discogs(1, &v))));
                }
            }
        }
        #[cfg(debug_assertions)]
        if let Ok(p) = std::env::var("NEEDLEDROP_REF") {
            app.refs.push((PathBuf::from(p), None));
        }
        app
    }

    // ---------- derived values ----------

    fn side_tracks(&self) -> Vec<meta::Track> {
        match &self.release {
            Some(r) => r.tracks_on(&self.side).into_iter().cloned().collect(),
            None => Vec::new(),
        }
    }

    fn ref_estimate(&self) -> Option<(f64, f64)> {
        let ok: Vec<Estimate> = self.refs.iter().filter_map(|(_, r)| r.as_ref()?.as_ref().ok().cloned()).collect();
        speed::combine(&ok)
    }

    fn db_estimate(&self) -> Option<(f64, f64, f64)> {
        let durs: Option<Vec<f64>> = self.side_tracks().iter().map(|t| t.duration).collect();
        let ts: Vec<f64> = self.markers.iter().map(|m| m.t).collect();
        split::db_factor(&ts, &durs?)
    }

    /// (factor, short description of how it was obtained)
    fn factor(&self) -> (f64, String) {
        match self.method {
            Method::Reference => match self.ref_estimate() {
                Some((f, _)) => (f, "reference cross-correlation".into()),
                None => (1.0, "no correction".into()),
            },
            Method::Database => match self.db_estimate() {
                Some((f, _, _)) => (f, "database side total".into()),
                None => (1.0, "no correction".into()),
            },
            Method::Manual => (100.0 / self.manual_speed.max(1.0), format!("manual {:.3} %", self.manual_speed)),
            Method::Off => (1.0, "no correction".into()),
        }
    }

    fn propose_markers(&mut self) {
        if let Some(an) = &self.analysis {
            let durs: Vec<Option<f64>> = self.side_tracks().iter().map(|t| t.duration).collect();
            if !durs.is_empty() {
                self.markers = split::propose(&durs, an);
                self.selected = None;
                if self.method == Method::Off || (self.method == Method::Database && self.db_estimate().is_none()) {
                    self.method = if self.ref_estimate().is_some() {
                        Method::Reference
                    } else if self.db_estimate().is_some() {
                        Method::Database
                    } else {
                        Method::Off
                    };
                }
            }
        }
    }

    // ---------- background jobs ----------

    fn fetch_release(&mut self, ctx: &egui::Context) {
        let input = self.release_input.clone();
        let token = self.prefs.discogs_token.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.fetching = true;
        self.error = None;
        std::thread::spawn(move || {
            let r = meta::fetch(&input, Some(&token)).map_err(|e| format!("{e:#}"));
            let cover_url = r.as_ref().ok().and_then(|r| r.cover_url.clone());
            let _ = tx.send(Msg::Release(r));
            ctx.request_repaint();
            let cover = cover_url.and_then(|u| meta::fetch_bytes(&u, Some(&token)).ok());
            let _ = tx.send(Msg::Cover(cover));
            ctx.request_repaint();
        });
    }

    fn load_recording(&mut self, path: PathBuf, ctx: &egui::Context) {
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.loading = true;
        self.error = None;
        std::thread::spawn(move || {
            let r = audio::load(&path)
                .map(|a| {
                    let an = analyse(&a);
                    let f = speed::rec_features(&a);
                    (path, Arc::new(a), an, Arc::new(f))
                })
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Recording(r));
            ctx.request_repaint();
        });
    }

    fn run_reference(&mut self, idx: usize, ctx: &egui::Context) {
        let Some(recf) = self.recf.clone() else { return };
        let path = self.refs[idx].0.clone();
        self.refs[idx].1 = None;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let r = audio::load(&path)
                .and_then(|a| speed::estimate_from_reference(&recf, &a))
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Reference(path, r));
            ctx.request_repaint();
        });
    }

    fn start_export(&mut self, ctx: &egui::Context) {
        let (Some(audio), Some(release)) = (self.audio.clone(), self.release.clone()) else { return };
        let tracks = self.side_tracks();
        if self.markers.len() != tracks.len() + 1 {
            return;
        }
        let cuts: Vec<Cut> = tracks
            .iter()
            .enumerate()
            .map(|(i, t)| Cut { track: t.clone(), start: self.markers[i].t, end: self.markers[i + 1].t })
            .collect();
        let (factor, method) = self.factor();
        let bits = if self.prefs.bits == 0 { if audio.bits > 16 { 24 } else { 16 } } else { self.prefs.bits };
        let settings = Settings {
            root: PathBuf::from(self.prefs.output_root.trim()),
            bits,
            embed_cover: self.prefs.embed_cover,
            overwrite: self.prefs.overwrite,
            lead_in: 0.3,
            tail: 1.5,
        };
        let cover = self.cover.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.exporting = Some((0.0, "Starting…".into()));
        self.report = None;
        std::thread::spawn(move || {
            let tx2 = tx.clone();
            let ctx2 = ctx.clone();
            let progress = move |f: f32, m: String| {
                let _ = tx2.send(Msg::Progress(f, m));
                ctx2.request_repaint();
            };
            let r = export::export(&audio, &release, &cuts, factor, &method, &settings, cover, &progress)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Exported(r));
            ctx.request_repaint();
        });
    }

    fn handle_messages(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Release(r) => {
                    self.fetching = false;
                    match r {
                        Ok(rel) => {
                            self.side = rel.sides().first().cloned().unwrap_or_default();
                            self.release = Some(rel);
                            self.cover = None;
                            self.propose_markers();
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
                Msg::Cover(c) => self.cover = c,
                Msg::Recording(r) => {
                    self.loading = false;
                    match r {
                        Ok((p, a, an, f)) => {
                            self.view = (0.0, a.duration());
                            self.rec_path = Some(p);
                            self.audio = Some(a);
                            self.analysis = Some(an);
                            self.recf = Some(f);
                            self.markers.clear();
                            self.propose_markers();
                            for i in 0..self.refs.len() {
                                self.run_reference(i, ctx);
                            }
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
                Msg::Reference(p, r) => {
                    if let Some(slot) = self.refs.iter_mut().find(|(q, _)| *q == p) {
                        slot.1 = Some(r);
                    }
                    if self.ref_estimate().is_some() && matches!(self.method, Method::Off | Method::Database) {
                        self.method = Method::Reference;
                    }
                }
                Msg::Progress(f, m) => self.exporting = Some((f, m)),
                Msg::Exported(r) => {
                    self.exporting = None;
                    self.report = Some(r.map(|rep| (rep.folder, rep.files.len(), rep.clipped_samples)));
                }
            }
        }
    }

    // ---------- playback ----------

    fn play(&mut self, from: f64, secs: f64) {
        let Some(a) = self.audio.clone() else { return };
        let (factor, _) = self.factor();
        if self.stream.is_none() {
            match rodio::OutputStream::try_default() {
                Ok(s) => self.stream = Some(s),
                Err(e) => {
                    self.error = Some(format!("audio output: {e}"));
                    return;
                }
            }
        }
        let Some((_, handle)) = &self.stream else { return };
        let rate = a.rate as f64;
        let from = from.max(0.0);
        let out_len = (secs * rate) as usize;
        let chans: Vec<&[f32]> = a.data.iter().map(|c| c.as_slice()).collect();
        let planar = resample(&chans, from * rate, factor, out_len, &Kernel::new(factor));
        let nch = planar.len();
        let mut inter = Vec::with_capacity(out_len * nch);
        for i in 0..out_len {
            for c in &planar {
                inter.push(c[i]);
            }
        }
        if let Some(s) = self.sink.take() {
            s.stop();
        }
        if let Ok(sink) = rodio::Sink::try_new(handle) {
            sink.append(rodio::buffer::SamplesBuffer::new(nch as u16, a.rate, inter));
            self.sink = Some(sink);
        }
    }

    fn stop(&mut self) {
        if let Some(s) = self.sink.take() {
            s.stop();
        }
    }

    // ---------- UI: left panel ----------

    fn ui_steps(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            heading(ui, "1  Release");
            ui.horizontal(|ui| {
                let te = ui.add(
                    egui::TextEdit::singleline(&mut self.release_input)
                        .hint_text("Discogs release URL/ID or MusicBrainz release URL")
                        .desired_width(ui.available_width() - 70.0),
                );
                let enter = te.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (ui.add_enabled(!self.fetching, egui::Button::new("Fetch")).clicked() || enter)
                    && !self.release_input.trim().is_empty()
                {
                    self.fetch_release(ctx);
                }
            });
            if self.fetching {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Looking up release…");
                });
            }
            if let Some(rel) = &self.release {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(format!("{} — {}", rel.artist, rel.title)).strong().size(15.0));
                let mut bits = Vec::new();
                if let Some(l) = &rel.label {
                    bits.push(l.clone());
                }
                if let Some(c) = &rel.catno {
                    bits.push(c.clone());
                }
                if let Some(y) = &rel.release_year {
                    bits.push(format!("pressed {y}"));
                }
                if let Some(y) = &rel.original_year {
                    bits.push(format!("original {y}"));
                }
                if let Some(c) = &rel.country {
                    bits.push(c.clone());
                }
                ui.label(egui::RichText::new(bits.join(" · ")).weak());
                ui.horizontal(|ui| {
                    ui.hyperlink_to("Open on the web", rel.url());
                    ui.label(egui::RichText::new(if self.cover.is_some() { "· cover art ✓" } else { "· no cover art" }).weak());
                });
                let sides = rel.sides();
                let mut side = self.side.clone();
                egui::ComboBox::from_label("Side in this recording")
                    .selected_text(side_label(&side))
                    .show_ui(ui, |ui| {
                        for s in &sides {
                            ui.selectable_value(&mut side, s.clone(), side_label(s));
                        }
                    });
                if side != self.side {
                    self.side = side;
                    self.propose_markers();
                }
                let tracks = self.side_tracks();
                egui::Grid::new("tl").striped(true).num_columns(3).show(ui, |ui| {
                    for t in &tracks {
                        ui.label(&t.position);
                        ui.label(&t.title);
                        ui.label(t.duration.map(fmt_secs).unwrap_or_else(|| "—".into()));
                        ui.end_row();
                    }
                });
                if tracks.iter().any(|t| t.duration.is_none()) {
                    warn(ui, "Some tracks have no length in the database: splits use the largest gaps, and the database speed method is unavailable.");
                }
            }

            ui.add_space(10.0);
            heading(ui, "2  Side recording");
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.loading, egui::Button::new("Open recording…")).clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("Audio", &["flac", "wav", "aiff", "aif"])
                        .pick_file()
                    {
                        self.load_recording(p, ctx);
                    }
                }
                if self.loading {
                    ui.spinner();
                    ui.label("Loading & analysing…");
                }
            });
            if let (Some(a), Some(an), Some(p)) = (&self.audio, &self.analysis, &self.rec_path) {
                ui.label(p.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default());
                let peak_db = 20.0 * a.peak().max(1e-9).log10();
                ui.label(
                    egui::RichText::new(format!(
                        "{:.1} kHz · {}-bit · {} ch · {} · peak {:.1} dBFS",
                        a.rate as f64 / 1000.0,
                        a.bits,
                        a.channels(),
                        fmt_secs(a.duration()),
                        peak_db
                    ))
                    .weak(),
                );
                ui.label(
                    egui::RichText::new(format!(
                        "Surface noise {:.0} dB · music {} to {} · {} gaps found",
                        an.noise_floor_db,
                        fmt_time(an.music_start),
                        fmt_time(an.music_end),
                        an.gaps.len()
                    ))
                    .weak(),
                );
                let clipped = a.clipped_samples();
                if clipped > 0 {
                    warn(ui, &format!("{clipped} samples at full scale: the recording clipped. Re-record with less gain."));
                } else if peak_db < -20.0 {
                    warn(ui, "Recording level is low; aim for peaks around −6 dBFS.");
                }
            }

            ui.add_space(10.0);
            heading(ui, "3  Speed");
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(self.recf.is_some(), egui::Button::new("Add reference copy…"))
                    .on_hover_text("A CD rip or download of one or more tracks on this side (any format). More tracks = more certainty.")
                    .on_disabled_hover_text("Load the side recording first")
                    .clicked()
                {
                    if let Some(ps) = rfd::FileDialog::new()
                        .add_filter("Audio", &["flac", "wav", "mp3", "m4a", "aac", "ogg", "aiff", "aif"])
                        .pick_files()
                    {
                        for p in ps {
                            if !self.refs.iter().any(|(q, _)| *q == p) {
                                self.refs.push((p, None));
                                let i = self.refs.len() - 1;
                                self.run_reference(i, ctx);
                            }
                        }
                    }
                }
            });
            let mut remove = None;
            for (i, (p, r)) in self.refs.iter().enumerate() {
                ui.horizontal_wrapped(|ui| {
                    if ui.small_button("x").on_hover_text("Remove").clicked() {
                        remove = Some(i);
                    }
                    ui.label(p.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default());
                    match r {
                        None => {
                            ui.spinner();
                        }
                        Some(Ok(e)) => {
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} ±{:.4} % · {}/{} anchors · r={:.2} · found at {}",
                                    turntable_speed(e.factor),
                                    e.stderr * 100.0,
                                    e.points_used,
                                    e.points_total,
                                    e.mean_corr,
                                    fmt_time(e.offset)
                                ))
                                .color(Color32::from_rgb(140, 200, 140)),
                            );
                        }
                        Some(Err(e)) => {
                            ui.label(egui::RichText::new(e).color(Color32::from_rgb(230, 120, 100)));
                        }
                    }
                });
            }
            if let Some(i) = remove {
                self.refs.remove(i);
            }

            let refe = self.ref_estimate();
            let dbe = self.db_estimate();
            ui.add_space(4.0);
            ui.add_enabled_ui(refe.is_some(), |ui| {
                let label = match refe {
                    Some((f, se)) => format!("Reference: {} ±{:.4} %", turntable_speed(f), se * 100.0),
                    None => "Reference: add a CD rip / download of a track".into(),
                };
                ui.radio_value(&mut self.method, Method::Reference, label);
            });
            ui.add_enabled_ui(dbe.is_some(), |ui| {
                let label = match dbe {
                    Some((f, se, slope)) => format!(
                        "Database side total: {} ±{:.2} %   (per-track fit {:.2} %)",
                        turntable_speed(f),
                        se * 100.0,
                        100.0 / slope
                    ),
                    None => "Database side total: needs track lengths and a recording".into(),
                };
                ui.radio_value(&mut self.method, Method::Database, label);
            });
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.method, Method::Manual, "Manual: turntable ran at");
                if ui
                    .add(egui::DragValue::new(&mut self.manual_speed).speed(0.001).range(90.0..=110.0).max_decimals(4).suffix(" %"))
                    .changed()
                {
                    self.method = Method::Manual;
                }
            });
            ui.radio_value(&mut self.method, Method::Off, "No speed correction");
            let (factor, _) = self.factor();
            ui.add_space(2.0);
            ui.label(egui::RichText::new(speed::describe(factor)).strong());
            if let (Some((rf, _)), Some((df, dse, _))) = (refe, dbe) {
                if (rf - df).abs() > 3.0 * dse {
                    warn(ui, "Reference and database disagree. Database lengths may be from a CD edition or include long fades; trust the reference.");
                }
            }

            ui.add_space(10.0);
            heading(ui, "4  Export");
            ui.horizontal(|ui| {
                ui.label("Library folder");
                ui.add(egui::TextEdit::singleline(&mut self.prefs.output_root).desired_width(ui.available_width() - 80.0));
                if ui.button("Browse…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        self.prefs.output_root = p.display().to_string();
                    }
                }
            });
            if let Some(rel) = &self.release {
                let folder = export::album_folder(Path::new(self.prefs.output_root.trim()), rel);
                ui.label(egui::RichText::new(format!("Saves to {}", folder.display())).weak().small());
            }
            ui.horizontal(|ui| {
                let src = self.audio.as_ref().map(|a| a.bits).unwrap_or(24);
                egui::ComboBox::from_label("")
                    .selected_text(match self.prefs.bits {
                        0 => format!("Same as recording ({}-bit)", if src > 16 { 24 } else { 16 }),
                        b => format!("{b}-bit"),
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.prefs.bits, 0, "Same as recording");
                        ui.selectable_value(&mut self.prefs.bits, 24, "24-bit");
                        ui.selectable_value(&mut self.prefs.bits, 16, "16-bit (dithered)");
                    });
                ui.checkbox(&mut self.prefs.embed_cover, "Embed cover");
                ui.checkbox(&mut self.prefs.overwrite, "Overwrite existing files");
            });
            let n = self.side_tracks().len();
            let ready = self.audio.is_some() && self.release.is_some() && n > 0 && self.markers.len() == n + 1 && self.exporting.is_none();
            ui.add_space(4.0);
            if ui
                .add_enabled(ready, egui::Button::new(egui::RichText::new(format!("Export {n} tracks")).strong()).min_size(Vec2::new(160.0, 28.0)))
                .clicked()
            {
                self.start_export(ctx);
            }
            if let Some((f, m)) = &self.exporting {
                ui.add(egui::ProgressBar::new(*f).text(m.as_str()));
            }
            match &self.report {
                Some(Ok((folder, files, clipped))) => {
                    ui.label(egui::RichText::new(format!("✓ {files} tracks written")).color(Color32::from_rgb(140, 200, 140)));
                    if *clipped > 0 {
                        warn(ui, &format!("{clipped} samples clipped during conversion (recording is very hot)."));
                    }
                    if ui.button("Open folder").clicked() {
                        open_folder(folder);
                    }
                }
                Some(Err(e)) => {
                    ui.label(egui::RichText::new(e).color(Color32::from_rgb(230, 120, 100)));
                }
                None => {}
            }

            ui.add_space(10.0);
            egui::CollapsingHeader::new("Settings").show(ui, |ui| {
                ui.label("Discogs personal access token (optional; gives cover art and higher rate limits):");
                ui.add(egui::TextEdit::singleline(&mut self.prefs.discogs_token).password(true).desired_width(f32::INFINITY));
                ui.hyperlink_to("Get one at discogs.com/settings/developers", "https://www.discogs.com/settings/developers");
            });

            if let Some(e) = &self.error {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(e).color(Color32::from_rgb(230, 120, 100)));
            }
        });
    }

    // ---------- UI: waveform + track table ----------

    fn ui_editor(&mut self, ui: &mut egui::Ui) {
        if self.audio.is_none() {
            ui.centered_and_justified(|ui| {
                ui.label(
                    egui::RichText::new(
                        "Fetch the release, then open the FLAC of one side.\n\nRecord each side as one file, 24-bit, peaks around −6 dBFS.",
                    )
                    .weak()
                    .size(15.0),
                );
            });
            return;
        }
        let dur = self.audio.as_ref().unwrap().duration();

        ui.horizontal(|ui| {
            if ui.button("Fit").clicked() {
                self.view = (0.0, dur);
            }
            if ui.button("Re-detect splits").clicked() {
                self.propose_markers();
            }
            ui.separator();
            let sel = self.selected;
            if ui.add_enabled(sel.is_some(), egui::Button::new("▶ Around marker")).clicked() {
                let t = self.markers[sel.unwrap()].t;
                self.play(t - 3.0, 6.0);
            }
            if ui.add_enabled(sel.is_some(), egui::Button::new("Zoom to marker")).clicked() {
                let t = self.markers[sel.unwrap()].t;
                self.view = ((t - 5.0).max(0.0), (t + 5.0).min(dur));
            }
            if ui.button("■ Stop").clicked() {
                self.stop();
            }
            ui.separator();
            ui.label(egui::RichText::new("Scroll = zoom · drag background = pan · drag marker = move · arrow keys nudge 10 ms (Shift 100 ms) · double-click = play").weak().small());
        });

        let height = 230.0;
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click_and_drag());
        self.waveform(ui, rect, &resp, dur);

        ui.add_space(6.0);
        self.track_table(ui);
    }

    fn waveform(&mut self, ui: &mut egui::Ui, rect: Rect, resp: &egui::Response, dur: f64) {
        let Some(an) = &self.analysis else { return };
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, Color32::from_rgb(22, 24, 28));
        let (v0, v1) = self.view;
        let span = (v1 - v0).max(0.01);
        let w = rect.width() as f64;
        let x_of = |t: f64| rect.left() + ((t - v0) / span * w) as f32;
        let t_of = |x: f32| v0 + ((x - rect.left()) as f64 / w) * span;
        let mid = rect.center().y;
        let half = rect.height() * 0.42;

        // gaps
        for g in &an.gaps {
            if g.end < v0 || g.start > v1 {
                continue;
            }
            let r = Rect::from_x_y_ranges(x_of(g.start).max(rect.left())..=x_of(g.end).min(rect.right()), rect.y_range());
            painter.rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(80, 120, 200, 28));
        }
        // waveform (peak per pixel column)
        let col = Color32::from_rgb(120, 170, 210);
        let n = an.peaks.len();
        for px in 0..rect.width() as usize {
            let ta = v0 + px as f64 / w * span;
            let tb = v0 + (px + 1) as f64 / w * span;
            let a = ((ta / HOP) as usize).min(n);
            let b = ((tb / HOP).ceil() as usize).clamp(a + 1, n.max(a + 1)).min(n);
            if a >= b {
                continue;
            }
            let p = an.peaks[a..b].iter().fold(0.0f32, |m, v| m.max(*v));
            let h = (p.min(1.0) * half).max(0.5);
            let x = rect.left() + px as f32 + 0.5;
            painter.line_segment([Pos2::new(x, mid - h), Pos2::new(x, mid + h)], Stroke::new(1.0_f32, col));
        }
        // time ruler
        let step = nice_step(span / 8.0);
        let mut t = (v0 / step).ceil() * step;
        while t < v1 {
            let x = x_of(t);
            painter.line_segment([Pos2::new(x, rect.bottom() - 6.0), Pos2::new(x, rect.bottom())], Stroke::new(1.0_f32, Color32::GRAY));
            painter.text(Pos2::new(x + 2.0, rect.bottom() - 4.0), Align2::LEFT_BOTTOM, fmt_time(t), FontId::proportional(10.0), Color32::GRAY);
            t += step;
        }
        // track labels
        let tracks = self.side_tracks();
        if self.markers.len() == tracks.len() + 1 {
            for (i, tr) in tracks.iter().enumerate() {
                let a = x_of(self.markers[i].t).max(rect.left());
                let b = x_of(self.markers[i + 1].t).min(rect.right());
                if b - a > 30.0 {
                    let txt = format!("{} {}", tr.position, tr.title);
                    let galley = painter.layout(txt, FontId::proportional(11.0), Color32::from_gray(200), b - a - 8.0);
                    painter.galley(Pos2::new(a + 4.0, rect.top() + 4.0), galley, Color32::WHITE);
                }
            }
        }
        // markers
        for (i, m) in self.markers.iter().enumerate() {
            if m.t < v0 || m.t > v1 {
                continue;
            }
            let x = x_of(m.t);
            let c = marker_color(m.kind);
            let width = if Some(i) == self.selected { 2.5_f32 } else { 1.5_f32 };
            painter.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], Stroke::new(width, c));
            painter.add(egui::Shape::convex_polygon(
                vec![Pos2::new(x - 6.0, rect.top()), Pos2::new(x + 6.0, rect.top()), Pos2::new(x, rect.top() + 9.0)],
                c,
                Stroke::NONE,
            ));
        }

        // ---- interaction ----
        let near_marker = |x: f32, markers: &[Marker]| {
            markers
                .iter()
                .enumerate()
                .map(|(i, m)| (i, (x_of(m.t) - x).abs()))
                .filter(|(_, d)| *d < 7.0)
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i)
        };
        if let Some(pos) = resp.hover_pos() {
            let t = t_of(pos.x);
            painter.text(Pos2::new(rect.right() - 6.0, rect.top() + 4.0), Align2::RIGHT_TOP, fmt_time(t), FontId::monospace(11.0), Color32::LIGHT_GRAY);
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                let z = (-(scroll as f64) * 0.003).exp();
                let new_span = (span * z).clamp(0.2, dur.max(0.2));
                let frac = (t - v0) / span;
                let mut a = t - frac * new_span;
                a = a.clamp(0.0, (dur - new_span).max(0.0));
                self.view = (a, a + new_span);
            }
            if near_marker(pos.x, &self.markers).is_some() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
        }
        if resp.drag_started() {
            if let Some(pos) = resp.interact_pointer_pos() {
                self.drag = near_marker(pos.x, &self.markers);
                if self.drag.is_some() {
                    self.selected = self.drag;
                }
            }
        }
        if resp.dragged() {
            match self.drag {
                Some(i) => {
                    if let Some(pos) = resp.interact_pointer_pos() {
                        let lo = if i > 0 { self.markers[i - 1].t + 0.1 } else { 0.0 };
                        let hi = if i + 1 < self.markers.len() { self.markers[i + 1].t - 0.1 } else { dur };
                        self.markers[i].t = t_of(pos.x).clamp(lo, hi);
                        if matches!(self.markers[i].kind, MarkerKind::Gap | MarkerKind::NoGap) {
                            self.markers[i].kind = MarkerKind::Manual;
                        }
                    }
                }
                None => {
                    let dt = -(resp.drag_delta().x as f64) / w * span;
                    let a = (v0 + dt).clamp(0.0, (dur - span).max(0.0));
                    self.view = (a, a + span);
                }
            }
        }
        if resp.drag_stopped() {
            self.drag = None;
        }
        if resp.clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                self.selected = near_marker(pos.x, &self.markers);
            }
        }
        if resp.double_clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                self.play(t_of(pos.x), 8.0);
            }
        }
        if let Some(i) = self.selected {
            if !ui.ctx().wants_keyboard_input() && i < self.markers.len() {
                let (l, r, shift) = ui.input(|inp| (inp.key_pressed(egui::Key::ArrowLeft), inp.key_pressed(egui::Key::ArrowRight), inp.modifiers.shift));
                let step = if shift { 0.1 } else { 0.01 };
                let d = if l { -step } else if r { step } else { 0.0 };
                if d != 0.0 {
                    let lo = if i > 0 { self.markers[i - 1].t + 0.1 } else { 0.0 };
                    let hi = if i + 1 < self.markers.len() { self.markers[i + 1].t - 0.1 } else { dur };
                    self.markers[i].t = (self.markers[i].t + d).clamp(lo, hi);
                    if matches!(self.markers[i].kind, MarkerKind::Gap | MarkerKind::NoGap) {
                        self.markers[i].kind = MarkerKind::Manual;
                    }
                }
            }
        }
    }

    fn track_table(&mut self, ui: &mut egui::Ui) {
        let tracks = self.side_tracks();
        if tracks.is_empty() {
            ui.label(egui::RichText::new("Fetch the release to place track boundaries from the database.").weak());
            return;
        }
        if self.markers.len() != tracks.len() + 1 {
            ui.label("Press “Re-detect splits”.");
            return;
        }
        let (factor, _) = self.factor();
        let ts: Vec<f64> = self.markers.iter().map(|m| m.t).collect();
        let durs: Vec<Option<f64>> = tracks.iter().map(|t| t.duration).collect();
        let deltas = split::track_deltas(&ts, &durs, factor);
        let mut action: Option<(f64, f64)> = None;
        let mut select: Option<usize> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("tracks").striped(true).num_columns(8).spacing([14.0, 6.0]).show(ui, |ui| {
                for h in ["", "Track", "Starts at", "Length", "Database", "Δ", "Boundary", "Listen"] {
                    ui.label(egui::RichText::new(h).strong());
                }
                ui.end_row();
                for (i, t) in tracks.iter().enumerate() {
                    ui.label(&t.position);
                    if ui.selectable_label(self.selected == Some(i), &t.title).clicked() {
                        select = Some(i);
                    }
                    ui.monospace(fmt_time(self.markers[i].t));
                    let (len, db, d) = deltas[i];
                    ui.monospace(fmt_secs(len));
                    ui.monospace(db.map(fmt_secs).unwrap_or_else(|| "—".into()));
                    match d {
                        Some(d) => {
                            let c = if d.abs() > 4.0 { Color32::from_rgb(230, 150, 90) } else { Color32::GRAY };
                            ui.label(egui::RichText::new(format!("{d:+.1} s")).monospace().color(c));
                        }
                        None => {
                            ui.label("");
                        }
                    }
                    let k = self.markers[i].kind;
                    ui.label(egui::RichText::new(kind_label(k)).color(marker_color(k)));
                    ui.horizontal(|ui| {
                        if ui.small_button("▶ start").on_hover_text("Play the first 6 s").clicked() {
                            action = Some((self.markers[i].t - 0.5, 6.0));
                        }
                        if ui.small_button("▶ end").on_hover_text("Play the last 6 s").clicked() {
                            action = Some((self.markers[i + 1].t - 6.0, 6.5));
                        }
                    });
                    ui.end_row();
                }
            });
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "Δ compares the speed-corrected length with the database. A single large Δ usually means a wrong database length or a split in the wrong gap, not a speed problem.",
                )
                .weak()
                .small(),
            );
        });
        if let Some(i) = select {
            self.selected = Some(i);
            let t = self.markers[i].t;
            let dur = self.audio.as_ref().map(|a| a.duration()).unwrap_or(t + 5.0);
            self.view = ((t - 5.0).max(0.0), (t + 5.0).min(dur));
        }
        if let Some((from, secs)) = action {
            self.play(from, secs);
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_messages(ctx);
        egui::SidePanel::left("steps").resizable(true).default_width(430.0).min_width(360.0).show(ctx, |ui| {
            ui.add_space(6.0);
            ui.label(egui::RichText::new("JPVinylRec").size(20.0).strong());
            ui.label(egui::RichText::new("Vinyl side to speed-corrected, split, tagged FLAC").weak());
            ui.separator();
            self.ui_steps(ui, ctx);
        });
        egui::CentralPanel::default().show(ctx, |ui| self.ui_editor(ui));
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.prefs);
    }
}

// ---------- helpers ----------

fn heading(ui: &mut egui::Ui, s: &str) {
    ui.label(egui::RichText::new(s).size(16.0).strong().color(Color32::from_rgb(230, 190, 110)));
}

fn warn(ui: &mut egui::Ui, s: &str) {
    ui.label(egui::RichText::new(format!("⚠ {s}")).color(Color32::from_rgb(230, 170, 80)));
}

fn side_label(s: &str) -> String {
    if s.len() <= 2 && s.chars().all(|c| c.is_ascii_alphabetic()) {
        format!("Side {s}")
    } else {
        s.to_string()
    }
}

fn fmt_secs(s: f64) -> String {
    let s = s.max(0.0);
    format!("{}:{:02}", (s / 60.0).floor() as u64, (s % 60.0).round() as u64 % 60)
}

/// "turntable 99.137 %" style description.
fn turntable_speed(factor: f64) -> String {
    format!("turntable {:.3} %", 100.0 / factor)
}

fn nice_step(raw: f64) -> f64 {
    for s in [0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0] {
        if s >= raw {
            return s;
        }
    }
    600.0
}

fn marker_color(k: MarkerKind) -> Color32 {
    match k {
        MarkerKind::Start | MarkerKind::End => Color32::from_rgb(120, 210, 120),
        MarkerKind::Gap => Color32::from_rgb(240, 200, 90),
        MarkerKind::NoGap => Color32::from_rgb(240, 90, 80),
        MarkerKind::Manual => Color32::from_rgb(200, 140, 240),
    }
}

fn kind_label(k: MarkerKind) -> &'static str {
    match k {
        MarkerKind::Start => "music start",
        MarkerKind::Gap => "in gap",
        MarkerKind::NoGap => "no gap — check",
        MarkerKind::Manual => "moved",
        MarkerKind::End => "",
    }
}

fn open_folder(p: &Path) {
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(p).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(p).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(p).spawn();
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("JPVinylRec")
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([980.0, 620.0]),
        ..Default::default()
    };
    eframe::run_native("JPVinylRec", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

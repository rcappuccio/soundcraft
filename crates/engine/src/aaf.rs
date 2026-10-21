//! Bounded AAF timeline import.
//!
//! This is deliberately limited to the composition structure exposed by
//! `oximedia-aaf`: audio tracks, source clips, their edit rates, and external
//! WAV files named by source mobs. It never reads PTX data or decodes embedded
//! AAF essence.

use crate::io::ConfinedHandle;
use crate::{Engine, EngineError, Result};
use oximedia_aaf::composition::{SequenceComponent, TrackType};
use oximedia_aaf::edit_session::AafEditSession;
use serde_json::{Value, json};
use soundcraft_model::{ChannelFormat, Clip, Source, SourceId, TrackKind};
use soundcraft_time::{MAX_POSITION, SampleRate, Samples};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// AAF containers with embedded essence are intentionally outside this first
/// importer's scope. The structure itself is normally much smaller.
const MAX_AAF_FILE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_AAF_METADATA_STREAM_BYTES: u64 = 4 * 1024 * 1024;
/// AAF's compound-file metadata uses one directory entry per small property/dictionary stream,
/// not per track or clip: an ordinary single-composition Pro Tools export of under 2 MiB already
/// needs several thousand (observed: 3,796 for a ~1.1 MiB session). This leaves generous headroom
/// for larger sessions while still bounding the directory-entry table's size and walk time.
const MAX_AAF_DIRECTORY_ENTRIES: usize = 65_536;
const MAX_AAF_COMPOSITIONS: usize = 64;
const MAX_AAF_TRACKS: usize = 256;
const MAX_AAF_CLIPS: usize = 10_000;
const MAX_AAF_MEDIA_BYTES: u64 = 128 * 1024 * 1024;
const MAX_AAF_MEDIA_SAMPLES: usize = 1 << 24;
const MAX_WARNINGS: usize = 200;
/// A Pro Tools (or other DAW) session report is a plain-text export, not session state; this
/// still bounds it so a hostile or enormous file cannot be read into memory wholesale.
const MAX_SESSION_REPORT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SESSION_REPORT_LINES: usize = 50_000;
const MAX_SESSION_REPORT_NOTES: usize = 200;
/// Case-insensitive substrings that mark a session-report line as migration-relevant. Matching
/// is deliberately shallow (no field parsing, no structured extraction): a line is either kept
/// verbatim as a note or it isn't, so this can never misreport what a proprietary binary contains.
const SESSION_REPORT_KEYWORDS: &[&str] =
    &["session name", "sample rate", "bit depth", "timecode", "track name", "plug-in", "plugin", "mute", "# of audio", "number of audio"];
const CFB_SECTOR_SIZE: u64 = 512;
const CFB_END_OF_CHAIN: u32 = 0xFFFF_FFFE;
const CFB_FREE_SECTOR: u32 = 0xFFFF_FFFF;
const CFB_FAT_SECTOR: u32 = 0xFFFF_FFFD;
const CFB_DIFAT_SECTOR: u32 = 0xFFFF_FFFC;

#[derive(Debug, Clone)]
pub struct AafImportReport {
    pub composition: String,
    pub sample_rate: u32,
    pub audio_tracks: usize,
    pub clips: usize,
    pub linked_wav_files: usize,
    pub warnings: Vec<String>,
}

impl AafImportReport {
    pub fn json(&self) -> Value {
        json!({
            "composition": self.composition,
            "sample_rate": self.sample_rate,
            "audio_tracks": self.audio_tracks,
            "clips": self.clips,
            "linked_wav_files": self.linked_wav_files,
            "warnings": self.warnings,
        })
    }
}

#[derive(Debug, Clone)]
struct TrackPlan {
    name: String,
    clips: Vec<ClipPlan>,
}

#[derive(Debug, Clone)]
struct ClipPlan {
    source_key: String,
    source_name: String,
    name: String,
    start: Samples,
    offset: Samples,
    length: Samples,
}

#[derive(Debug, Clone, Copy)]
struct ImportedSource {
    id: SourceId,
    frames: Samples,
    channels: usize,
    loaded: bool,
}

#[derive(Debug, Clone)]
struct DirectoryEntry {
    name: String,
    kind: u8,
    left: u32,
    right: u32,
    child: u32,
    start_sector: u32,
    size: u64,
}

/// Replace the current document with an imported audio AAF composition.
///
/// The parser is run only after a conservative CFB preflight. The whole
/// document is assembled off-engine so malformed input cannot leave a partial
/// session behind.
pub fn import(engine: &mut Engine, input: &Path, media_dir: Option<&Path>) -> Result<AafImportReport> {
    let aaf = read_aaf(input)?;
    let mut compositions = aaf.file.composition_mobs();
    if compositions.is_empty() {
        return Err(EngineError::Unsupported(format!(
            "{}: the compound-file container was read successfully, but no AAF composition mob was found in it (also found {} master mobs, {} source mobs, {} essence streams); no SoundCraft session was written. This initial importer can only map AAF object metadata as exposed by the oximedia-aaf crate, which may not recognize every AAF-authoring application's metadata encoding; re-export the session or report this AAF file if you believe this is an error",
            input.display(),
            aaf.file.master_mobs().len(),
            aaf.file.source_mobs().len(),
            aaf.file.essence_data().len()
        )));
    }
    if compositions.len() > MAX_AAF_COMPOSITIONS {
        return Err(EngineError::Unsupported(format!("AAF has more than {MAX_AAF_COMPOSITIONS} compositions")));
    }
    compositions.sort_by(|a, b| a.name().cmp(b.name()));
    let composition = match compositions.first() {
        Some(composition) => *composition,
        None => {
            return Err(EngineError::Unsupported("the AAF reader exposed no composition timeline; no SoundCraft session was written".into()));
        }
    };

    let mut warnings = Vec::new();
    if compositions.len() > 1 {
        warning(
            &mut warnings,
            format!(
                "AAF contains {} compositions; imported `{}` (the first name in deterministic order)",
                compositions.len(),
                label(composition.name())
            ),
        );
    }

    let tracks: Vec<_> = composition.tracks().into_iter().filter(|track| track.track_type == TrackType::Sound).collect();
    if tracks.is_empty() {
        return Err(EngineError::Unsupported(
            "the AAF composition has no audio timeline tracks exposed by the parser; no SoundCraft session was written".into(),
        ));
    }
    if tracks.len() > MAX_AAF_TRACKS {
        return Err(EngineError::Unsupported(format!("AAF composition has more than {MAX_AAF_TRACKS} audio tracks")));
    }

    let sample_rate = session_rate(&tracks, &mut warnings)?;
    let session_name = nonempty_label(composition.name(), "AAF import");
    let mut session = soundcraft_model::Session::new(session_name.clone(), sample_rate);
    let plans = plan_tracks(&aaf.file, &tracks, sample_rate.hz(), &mut warnings)?;
    if plans.iter().all(|track| track.clips.is_empty()) {
        return Err(EngineError::Unsupported("the AAF audio tracks contain no supported source clips; no SoundCraft session was written".into()));
    }

    let mut required_frames: HashMap<String, Samples> = HashMap::new();
    let mut source_names: HashMap<String, String> = HashMap::new();
    for track in &plans {
        for clip in &track.clips {
            let end = clip.offset.saturating_add(clip.length).max(1);
            required_frames.entry(clip.source_key.clone()).and_modify(|frames| *frames = (*frames).max(end)).or_insert(end);
            source_names.entry(clip.source_key.clone()).or_insert_with(|| clip.source_name.clone());
        }
    }

    let mut sources = HashMap::new();
    let mut linked_wav_files = 0usize;
    for (source_key, required) in &required_frames {
        let source_name = source_names.get(source_key).map(String::as_str).unwrap_or("AAF source");
        let imported = match load_wav_source(&mut session, source_name, media_dir) {
            Ok(Some(imported)) => {
                linked_wav_files = linked_wav_files.saturating_add(1);
                imported
            }
            Ok(None) => {
                warning(&mut warnings, offline_source_warning(source_name, media_dir));
                add_offline_source(&mut session, source_name, offline_path(source_name, media_dir), *required)
            }
            Err(reason) => {
                warning(&mut warnings, format!("{}; leaving `{}` offline", reason, label(source_name)));
                add_offline_source(&mut session, source_name, offline_path(source_name, media_dir), *required)
            }
        };
        sources.insert(source_key.clone(), imported);
    }

    let mut clip_count = 0usize;
    for plan in plans {
        let track_format = plan
            .clips
            .iter()
            .filter_map(|clip| sources.get(&clip.source_key))
            .find(|source| source.loaded)
            .map_or(ChannelFormat::Mono, |source| ChannelFormat::for_channels(source.channels));
        let track_id = session.add_track(TrackKind::Audio, track_format, Some(&plan.name));
        for clip in plan.clips {
            let Some(source) = sources.get(&clip.source_key).copied() else {
                warning(&mut warnings, format!("AAF clip `{}` has no usable source reference", label(&clip.name)));
                continue;
            };
            let mut length = clip.length;
            if source.loaded {
                if clip.offset >= source.frames {
                    warning(
                        &mut warnings,
                        format!("AAF clip `{}` starts beyond decoded WAV `{}` and was skipped", label(&clip.name), label(&clip.source_name)),
                    );
                    continue;
                }
                let available = source.frames.saturating_sub(clip.offset);
                if length > available {
                    warning(
                        &mut warnings,
                        format!("AAF clip `{}` exceeds decoded WAV `{}` and was shortened", label(&clip.name), label(&clip.source_name)),
                    );
                    length = available;
                }
            }
            if length <= 0 {
                continue;
            }
            let id = session.new_clip_id();
            crate::edit::place_clip(&mut session, track_id, Clip::audio(id, clip.name, source.id, clip.offset, clip.start, length));
            clip_count = clip_count.saturating_add(1);
        }
    }

    if clip_count == 0 {
        return Err(EngineError::Unsupported("AAF source clips could not be represented safely; no SoundCraft session was written".into()));
    }
    for message in &warnings {
        engine.message(message.clone());
    }
    engine.replace_session(session);
    Ok(AafImportReport {
        composition: session_name,
        sample_rate: sample_rate.hz(),
        audio_tracks: tracks.len(),
        clips: clip_count,
        linked_wav_files,
        warnings,
    })
}

/// Read a Pro Tools (or other DAW) plain-text session report and keep only the lines that
/// mention a migration-relevant field, as safe, verbatim, user-facing notes.
///
/// This never reconstructs session state from the report and never touches the engine's
/// document: it only highlights lines a person migrating a session may want to read (sample
/// rate, bit depth, timecode, track names, mute state, plug-in listings) so they can follow up
/// manually. The report is treated purely as text; nothing here parses or assumes a specific
/// proprietary layout.
pub fn session_report_notes(path: &Path) -> Result<Vec<String>> {
    let len = crate::io::file_len(path)?;
    if len > MAX_SESSION_REPORT_BYTES {
        return Err(EngineError::Unsupported(format!(
            "{}: session reports larger than {} MiB are not accepted",
            path.display(),
            MAX_SESSION_REPORT_BYTES / (1024 * 1024)
        )));
    }
    let bytes = crate::io::read_file(path)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut notes = Vec::new();
    for line in text.lines().take(MAX_SESSION_REPORT_LINES) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if SESSION_REPORT_KEYWORDS.iter().any(|keyword| lower.contains(keyword)) {
            if notes.len() < MAX_SESSION_REPORT_NOTES.saturating_sub(1) {
                notes.push(label(trimmed));
            } else if notes.len() == MAX_SESSION_REPORT_NOTES.saturating_sub(1) {
                notes.push("further session-report notes were suppressed".into());
            }
        }
    }
    Ok(notes)
}

fn read_aaf(input: &Path) -> Result<AafEditSession> {
    let (mut file, len) = ConfinedHandle::open(input)?;
    if len > MAX_AAF_FILE_BYTES {
        return Err(EngineError::Unsupported(format!(
            "{}: AAF files larger than {} MiB are not accepted by this initial importer",
            input.display(),
            MAX_AAF_FILE_BYTES / (1024 * 1024)
        )));
    }
    let virtual_len = preflight_cfb(&mut file, len, input)?;
    file.seek(SeekFrom::Start(0)).map_err(|err| EngineError::Io(format!("{}: {err}", input.display())))?;
    let padded = PaddedCfbReader::new(file, len, virtual_len);
    AafEditSession::from_reader(padded).map_err(|err| {
        EngineError::Unsupported(format!(
            "{}: AAF structure could not be decoded by the supported importer ({err}); no SoundCraft session was written",
            input.display()
        ))
    })
}

/// Pads reads past a file's real length with zero bytes, up to `virtual_len`: the container's
/// own sector-aligned extent, which `preflight_cfb` has already proven safe to address. Several
/// real-world AAF writers (observed from a Pro Tools export) leave a file's very last addressable
/// sector physically short instead of padding it; `oximedia-aaf`'s own compound-file reader always
/// reads full, fixed-size sectors for its allocation tables, so without this wrapper it fails with
/// a bare I/O error on files this importer has already structurally validated as sound. Nothing is
/// ever read past `virtual_len`: that still ends the stream normally, so a genuinely truncated or
/// corrupt container is not hidden by this, only a writer's harmless trailing short sector is.
struct PaddedCfbReader {
    inner: ConfinedHandle,
    real_len: u64,
    virtual_len: u64,
    pos: u64,
}

impl PaddedCfbReader {
    fn new(inner: ConfinedHandle, real_len: u64, virtual_len: u64) -> Self {
        PaddedCfbReader { inner, real_len, virtual_len: virtual_len.max(real_len), pos: 0 }
    }
}

impl Read for PaddedCfbReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() || self.pos >= self.virtual_len {
            return Ok(0);
        }
        if self.pos < self.real_len {
            self.inner.seek(SeekFrom::Start(self.pos))?;
            let real_remaining = self.real_len - self.pos;
            let want = usize::try_from(real_remaining).unwrap_or(usize::MAX).min(buf.len());
            let read = self.inner.read(&mut buf[..want])?;
            self.pos = self.pos.saturating_add(read as u64);
            return Ok(read);
        }
        let pad_remaining = self.virtual_len - self.pos;
        let fill = usize::try_from(pad_remaining).unwrap_or(usize::MAX).min(buf.len());
        for byte in &mut buf[..fill] {
            *byte = 0;
        }
        self.pos = self.pos.saturating_add(fill as u64);
        Ok(fill)
    }
}

impl Seek for PaddedCfbReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(offset) => self.virtual_len.checked_add_signed(offset),
            SeekFrom::Current(offset) => self.pos.checked_add_signed(offset),
        };
        self.pos = new_pos.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek position overflows"))?;
        Ok(self.pos)
    }
}

fn session_rate(tracks: &[oximedia_aaf::Track], warnings: &mut Vec<String>) -> Result<SampleRate> {
    let mut rates =
        tracks.iter().filter_map(|track| rational_sample_rate(track.edit_rate.numerator, track.edit_rate.denominator)).collect::<Vec<_>>();
    rates.sort_unstable();
    rates.dedup();
    let rate = rates.first().copied().unwrap_or(48_000);
    if rates.is_empty() {
        warning(warnings, "AAF audio edit-rate metadata was absent or not a supported integer sample rate; using 48000 Hz".into());
    } else if rates.len() > 1 {
        warning(warnings, format!("AAF audio tracks declare multiple edit rates ({rates:?}); using {rate} Hz and resampling linked WAV files"));
    }
    SampleRate::new(rate).map_err(|err| EngineError::Unsupported(format!("AAF sample rate {rate}: {err}")))
}

fn rational_sample_rate(numerator: i32, denominator: i32) -> Option<u32> {
    let numerator = u32::try_from(numerator).ok()?;
    let denominator = u32::try_from(denominator).ok()?;
    if denominator == 0 || numerator % denominator != 0 {
        return None;
    }
    let rate = numerator / denominator;
    SampleRate::new(rate).ok().map(|rate| rate.hz())
}

fn plan_tracks(aaf: &oximedia_aaf::AafFile, tracks: &[oximedia_aaf::Track], session_rate: u32, warnings: &mut Vec<String>) -> Result<Vec<TrackPlan>> {
    let mut all = Vec::new();
    let mut total_clips = 0usize;
    for track in tracks {
        let mut cursor = track.origin.0;
        let mut clips = Vec::new();
        let Some(sequence) = &track.sequence else {
            warning(warnings, format!("AAF audio track `{}` has no sequence", label(&track.name)));
            all.push(TrackPlan { name: nonempty_label(&track.name, "AAF audio"), clips });
            continue;
        };
        if sequence.components.len() > MAX_AAF_CLIPS {
            return Err(EngineError::Unsupported(format!("AAF track `{}` has more than {MAX_AAF_CLIPS} components", label(&track.name))));
        }
        for component in &sequence.components {
            let length = component.length().unwrap_or(0);
            if length <= 0 {
                warning(warnings, format!("AAF track `{}` has a component with no positive duration", label(&track.name)));
                continue;
            }
            if let SequenceComponent::SourceClip(source) = component {
                let start = units_to_samples(cursor, track.edit_rate.numerator, track.edit_rate.denominator, session_rate)?;
                let offset = units_to_samples(source.start_time.0, track.edit_rate.numerator, track.edit_rate.denominator, session_rate)?;
                let duration = units_to_samples(length, track.edit_rate.numerator, track.edit_rate.denominator, session_rate)?;
                if offset < 0 || duration <= 0 {
                    warning(warnings, format!("AAF source clip on `{}` has an unsupported negative offset or empty duration", label(&track.name)));
                } else {
                    let source_key = source.source_mob_id.to_string();
                    let source_name = aaf
                        .find_mob(&source.source_mob_id)
                        .map(|mob| nonempty_label(mob.name(), "AAF source"))
                        .unwrap_or_else(|| format!("AAF source {source_key}"));
                    clips.push(ClipPlan { source_key, source_name: source_name.clone(), name: source_name, start, offset, length: duration });
                    total_clips = total_clips.saturating_add(1);
                    if total_clips > MAX_AAF_CLIPS {
                        return Err(EngineError::Unsupported(format!("AAF composition has more than {MAX_AAF_CLIPS} source clips")));
                    }
                }
            } else {
                warning(
                    warnings,
                    format!(
                        "AAF `{}` component on `{}` is not rendered by this initial importer; its declared duration is retained for following clips",
                        component_name(component),
                        label(&track.name)
                    ),
                );
            }
            cursor = cursor
                .checked_add(length)
                .ok_or_else(|| EngineError::Unsupported(format!("AAF track `{}` has a timeline position overflow", label(&track.name))))?;
        }
        all.push(TrackPlan { name: nonempty_label(&track.name, "AAF audio"), clips });
    }
    Ok(all)
}

fn component_name(component: &SequenceComponent) -> &'static str {
    match component {
        SequenceComponent::SourceClip(_) => "source clip",
        SequenceComponent::Filler(_) => "filler",
        SequenceComponent::Transition(_) => "transition",
        SequenceComponent::Effect(_) => "effect",
    }
}

fn units_to_samples(units: i64, numerator: i32, denominator: i32, session_rate: u32) -> Result<Samples> {
    if numerator <= 0 || denominator <= 0 {
        return Err(EngineError::Unsupported("AAF has an invalid audio edit rate".into()));
    }
    let sign = if units < 0 { -1i128 } else { 1i128 };
    let magnitude = i128::from(units).unsigned_abs();
    let scaled = magnitude
        .checked_mul(u128::try_from(denominator).unwrap_or(0))
        .and_then(|value| value.checked_mul(u128::from(session_rate)))
        .ok_or_else(|| EngineError::Unsupported("AAF timeline value is too large".into()))?;
    let divisor = u128::try_from(numerator).unwrap_or(1);
    let rounded = scaled
        .checked_add(divisor / 2)
        .map(|value| value / divisor)
        .ok_or_else(|| EngineError::Unsupported("AAF timeline value is too large".into()))?;
    let rounded = i128::try_from(rounded).map_err(|_| EngineError::Unsupported("AAF timeline value is too large".into()))?;
    let samples = rounded.checked_mul(sign).ok_or_else(|| EngineError::Unsupported("AAF timeline value is too large".into()))?;
    let samples = i64::try_from(samples).map_err(|_| EngineError::Unsupported("AAF timeline value is too large".into()))?;
    if !(-MAX_POSITION..=MAX_POSITION).contains(&samples) {
        return Err(EngineError::Unsupported(format!("AAF timeline value exceeds SoundCraft's {}-sample safety limit", MAX_POSITION)));
    }
    Ok(samples)
}

fn load_wav_source(
    session: &mut soundcraft_model::Session,
    source_name: &str,
    media_dir: Option<&Path>,
) -> std::result::Result<Option<ImportedSource>, String> {
    let Some(media_dir) = media_dir else {
        return Ok(None);
    };
    let Some(path) = wav_candidate(media_dir, source_name) else {
        return Ok(None);
    };
    crate::file_access::check_read(&path)?;
    let len = crate::io::file_len(&path).map_err(|err| err.to_string())?;
    if len > MAX_AAF_MEDIA_BYTES {
        return Err(format!("{} is larger than the {} MiB WAV import limit", path.display(), MAX_AAF_MEDIA_BYTES / (1024 * 1024)));
    }
    let bytes = crate::io::read_file(&path).map_err(|err| err.to_string())?;
    let (info, buffer) = soundcraft_audio_io::decode(&bytes, Some("wav")).map_err(|err| format!("{}: {err}", path.display()))?;
    if info.format != soundcraft_audio_io::FileFormat::Wav {
        return Err(format!("{} is not WAV audio", path.display()));
    }
    let buffer = crate::io::at_rate(buffer, session.sample_rate.hz(), MAX_AAF_MEDIA_SAMPLES)
        .ok_or_else(|| format!("{} decodes to too many samples", path.display()))?;
    if buffer.frames() == 0 || buffer.num_channels() == 0 {
        return Err(format!("{} has no audio", path.display()));
    }
    let frames = i64::try_from(buffer.frames()).map_err(|_| format!("{} is too long", path.display()))?;
    let channels = buffer.num_channels();
    let id = crate::io::add_source(session, source_name, buffer, Some(&path.to_string_lossy()), soundcraft_audio_io::FileFormat::Wav);
    Ok(Some(ImportedSource { id, frames, channels, loaded: true }))
}

fn wav_candidate(media_dir: &Path, source_name: &str) -> Option<PathBuf> {
    let file_name = Path::new(source_name).file_name()?;
    let is_wav = file_name
        .to_str()
        .and_then(|name| Path::new(name).extension())
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"));
    is_wav.then(|| media_dir.join(file_name))
}

fn offline_path(source_name: &str, media_dir: Option<&Path>) -> String {
    let file_name = Path::new(source_name)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "AAF source.wav".into());
    media_dir
        .map(|dir| dir.join(&file_name).to_string_lossy().into_owned())
        .unwrap_or_else(|| Path::new("AAF Media").join(file_name).to_string_lossy().into_owned())
}

fn offline_source_warning(source_name: &str, media_dir: Option<&Path>) -> String {
    match media_dir {
        Some(dir) if wav_candidate(dir, source_name).is_none() => {
            format!("AAF source `{}` is not named as a WAV file, so it was kept offline", label(source_name))
        }
        Some(dir) => {
            format!("AAF WAV `{}` was not found under `{}`; its clip reference was kept offline", label(source_name), label(&dir.to_string_lossy()))
        }
        None => format!("AAF source `{}` was kept offline; pass `--media-dir` containing its exact WAV filename to link it", label(source_name)),
    }
}

fn add_offline_source(session: &mut soundcraft_model::Session, name: &str, path: String, required_frames: Samples) -> ImportedSource {
    let id = SourceId(session.alloc());
    let frames = required_frames.max(1);
    session.sources.push(Source {
        id,
        name: nonempty_label(name, "AAF source"),
        path,
        channels: 1,
        frames: u64::try_from(frames).unwrap_or(u64::MAX),
        sample_rate: session.sample_rate.hz(),
        format: soundcraft_audio_io::FileFormat::Wav,
        time_reference: 0,
        unsaved: false,
    });
    ImportedSource { id, frames, channels: 1, loaded: false }
}

fn warning(warnings: &mut Vec<String>, message: String) {
    if warnings.len() < MAX_WARNINGS.saturating_sub(1) {
        warnings.push(message);
    } else if warnings.len() == MAX_WARNINGS.saturating_sub(1) {
        warnings.push("further AAF import warnings were suppressed".into());
    }
}

fn label(text: &str) -> String {
    const LIMIT: usize = 120;
    let mut text = text.chars().take(LIMIT).collect::<String>();
    if text.is_empty() {
        text = "unnamed".into();
    }
    text
}

fn nonempty_label(text: &str, fallback: &str) -> String {
    let text = label(text);
    if text == "unnamed" { fallback.into() } else { text }
}

fn preflight_cfb(file: &mut ConfinedHandle, file_len: u64, input: &Path) -> Result<u64> {
    if file_len < CFB_SECTOR_SIZE {
        return Err(EngineError::Unsupported(format!("{}: invalid CFB container length", input.display())));
    }
    // Several real-world AAF writers (observed from a Pro Tools export) stop writing a file
    // right after its last meaningful byte instead of padding the final sector out to
    // `CFB_SECTOR_SIZE`: the last addressable sector can be a few hundred bytes short. Counting
    // sectors by ceiling division (rather than requiring an exact multiple) makes that final,
    // partially-written sector addressable; `read_sector` separately zero-pads just that one
    // read instead of failing it. Every earlier sector is still required to be fully present:
    // the arithmetic below guarantees a short read is only ever possible for sector
    // `sector_count - 1`, so this never masks a genuinely truncated or corrupt container.
    let sector_count = usize::try_from((file_len - CFB_SECTOR_SIZE).div_ceil(CFB_SECTOR_SIZE))
        .map_err(|_| EngineError::Unsupported(format!("{}: CFB is too large", input.display())))?;
    if sector_count == 0 || sector_count > MAX_AAF_FILE_BYTES as usize / CFB_SECTOR_SIZE as usize {
        return Err(EngineError::Unsupported(format!("{}: invalid CFB sector count", input.display())));
    }

    let mut header = [0u8; CFB_SECTOR_SIZE as usize];
    file.seek(SeekFrom::Start(0)).and_then(|_| file.read_exact(&mut header)).map_err(|err| EngineError::Io(format!("{}: {err}", input.display())))?;
    if header.get(..8) != Some(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
        return Err(EngineError::Unsupported(format!("{}: not an AAF compound-file container", input.display())));
    }
    let major = le_u16(&header, 26)?;
    let byte_order = le_u16(&header, 28)?;
    let sector_shift = le_u16(&header, 30)?;
    let mini_sector_shift = le_u16(&header, 32)?;
    if !matches!(major, 3 | 4) || byte_order != 0xFFFE || sector_shift != 9 || mini_sector_shift != 6 {
        return Err(EngineError::Unsupported(format!("{}: unsupported CFB sector layout", input.display())));
    }
    if le_u32(&header, 56)? != 4096 {
        return Err(EngineError::Unsupported(format!("{}: unsupported CFB mini-stream cutoff", input.display())));
    }

    let fat_count =
        usize::try_from(le_u32(&header, 44)?).map_err(|_| EngineError::Unsupported(format!("{}: invalid CFB FAT count", input.display())))?;
    let directory_start = le_u32(&header, 48)?;
    let mini_fat_start = le_u32(&header, 60)?;
    let mini_fat_count =
        usize::try_from(le_u32(&header, 64)?).map_err(|_| EngineError::Unsupported(format!("{}: invalid CFB mini-FAT count", input.display())))?;
    let difat_start = le_u32(&header, 68)?;
    let difat_count =
        usize::try_from(le_u32(&header, 72)?).map_err(|_| EngineError::Unsupported(format!("{}: invalid CFB DIFAT count", input.display())))?;
    if fat_count > sector_count || mini_fat_count > sector_count || difat_count > sector_count {
        return Err(EngineError::Unsupported(format!("{}: impossible CFB allocation counts", input.display())));
    }

    let mut fat_sectors = Vec::new();
    for offset in (76..CFB_SECTOR_SIZE as usize).step_by(4) {
        let sector = le_u32(&header, offset)?;
        if sector != CFB_FREE_SECTOR {
            valid_sector(sector, sector_count, "FAT")?;
            fat_sectors.push(sector);
        }
    }
    let mut next_difat = difat_start;
    let mut seen_difat = HashSet::new();
    for index in 0..difat_count {
        valid_sector(next_difat, sector_count, "DIFAT")?;
        if !seen_difat.insert(next_difat) {
            return Err(EngineError::Unsupported("AAF CFB DIFAT has a cycle".into()));
        }
        let sector = read_sector(file, next_difat, input)?;
        for offset in (0..508).step_by(4) {
            let fat_sector = le_u32(&sector, offset)?;
            if fat_sector != CFB_FREE_SECTOR {
                valid_sector(fat_sector, sector_count, "FAT")?;
                fat_sectors.push(fat_sector);
            }
        }
        let following = le_u32(&sector, 508)?;
        if index.saturating_add(1) == difat_count {
            if !is_chain_end(following) {
                return Err(EngineError::Unsupported("AAF CFB DIFAT count does not terminate its chain".into()));
            }
        } else {
            valid_sector(following, sector_count, "DIFAT")?;
        }
        next_difat = following;
    }
    if difat_count == 0 && !is_chain_end(difat_start) {
        return Err(EngineError::Unsupported("AAF CFB has an unexpected DIFAT chain".into()));
    }
    if fat_sectors.len() != fat_count {
        return Err(EngineError::Unsupported("AAF CFB FAT count does not match its DIFAT".into()));
    }
    let mut unique_fat = HashSet::new();
    if fat_sectors.iter().any(|sector| !unique_fat.insert(*sector)) {
        return Err(EngineError::Unsupported("AAF CFB repeats a FAT sector".into()));
    }

    let mut fat = Vec::with_capacity(fat_sectors.len().saturating_mul(128));
    for sector_id in fat_sectors {
        let sector = read_sector(file, sector_id, input)?;
        for offset in (0..CFB_SECTOR_SIZE as usize).step_by(4) {
            fat.push(le_u32(&sector, offset)?);
        }
    }

    let directory_chain = walk_chain(directory_start, &fat, sector_count, "directory")?;
    if directory_chain.is_empty() {
        return Err(EngineError::Unsupported("AAF CFB has no directory stream".into()));
    }
    let mut entries = Vec::new();
    for sector_id in directory_chain {
        let sector = read_sector(file, sector_id, input)?;
        for offset in (0..CFB_SECTOR_SIZE as usize).step_by(128) {
            if entries.len() >= MAX_AAF_DIRECTORY_ENTRIES {
                return Err(EngineError::Unsupported(format!("AAF has more than {MAX_AAF_DIRECTORY_ENTRIES} compound-file entries")));
            }
            entries.push(parse_directory_entry(&sector, offset)?);
        }
    }
    let Some(root) = entries.first() else {
        return Err(EngineError::Unsupported("AAF CFB has no root directory entry".into()));
    };
    if root.kind != 5 {
        return Err(EngineError::Unsupported("AAF CFB has an invalid root directory entry".into()));
    }
    validate_directory_tree(&entries)?;

    let mini_fat_chain = if mini_fat_count == 0 {
        if !is_chain_end(mini_fat_start) {
            return Err(EngineError::Unsupported("AAF CFB has an unexpected mini-FAT chain".into()));
        }
        Vec::new()
    } else {
        let chain = walk_chain(mini_fat_start, &fat, sector_count, "mini-FAT")?;
        if chain.len() != mini_fat_count {
            return Err(EngineError::Unsupported("AAF CFB mini-FAT count does not match its chain".into()));
        }
        chain
    };
    let mut mini_fat = Vec::with_capacity(mini_fat_chain.len().saturating_mul(128));
    for sector_id in mini_fat_chain {
        let sector = read_sector(file, sector_id, input)?;
        for offset in (0..CFB_SECTOR_SIZE as usize).step_by(4) {
            mini_fat.push(le_u32(&sector, offset)?);
        }
    }

    validate_regular_stream(root, &fat, sector_count, "root mini-stream")?;
    for entry in &entries {
        if matches!(entry.name.as_str(), "Header" | "ContentStorage" | "EssenceData" | "UnknownProperties") {
            validate_stream(entry, root.size, &fat, &mini_fat, sector_count)?;
        }
    }
    // The container's full addressable length: header plus every sector `sector_count` lets a
    // reference reach. `sector_count` was already bounded well below `u64::MAX / CFB_SECTOR_SIZE`
    // by the sector-count limit above, so this cannot overflow.
    Ok(CFB_SECTOR_SIZE + (sector_count as u64) * CFB_SECTOR_SIZE)
}

fn validate_directory_tree(entries: &[DirectoryEntry]) -> Result<()> {
    let mut state = vec![0u8; entries.len()];
    let mut stack = vec![(0usize, false)];
    while let Some((entry_id, finishing)) = stack.pop() {
        let Some(state_at) = state.get_mut(entry_id) else {
            return Err(EngineError::Unsupported("AAF CFB directory reference is out of range".into()));
        };
        if finishing {
            *state_at = 2;
            continue;
        }
        if *state_at == 1 {
            return Err(EngineError::Unsupported("AAF CFB directory tree has a cycle".into()));
        }
        if *state_at == 2 {
            continue;
        }
        *state_at = 1;
        stack.push((entry_id, true));
        let Some(entry) = entries.get(entry_id) else {
            return Err(EngineError::Unsupported("AAF CFB directory reference is out of range".into()));
        };
        for reference in [entry.left, entry.right, entry.child] {
            if reference != CFB_FREE_SECTOR {
                let reference =
                    usize::try_from(reference).map_err(|_| EngineError::Unsupported("AAF CFB directory reference is out of range".into()))?;
                if reference >= entries.len() {
                    return Err(EngineError::Unsupported("AAF CFB directory reference is out of range".into()));
                }
                stack.push((reference, false));
            }
        }
    }
    Ok(())
}

fn validate_stream(entry: &DirectoryEntry, root_size: u64, fat: &[u32], mini_fat: &[u32], sector_count: usize) -> Result<()> {
    if entry.kind != 2 {
        return Err(EngineError::Unsupported(format!("AAF CFB `{}` is not a stream", label(&entry.name))));
    }
    if entry.size > MAX_AAF_METADATA_STREAM_BYTES {
        return Err(EngineError::Unsupported(format!(
            "AAF `{}` stream exceeds the {} MiB metadata limit",
            label(&entry.name),
            MAX_AAF_METADATA_STREAM_BYTES / (1024 * 1024)
        )));
    }
    if entry.size == 0 {
        return Ok(());
    }
    if entry.size < 4096 {
        let chain = walk_chain(entry.start_sector, mini_fat, mini_fat.len(), "mini-stream")?;
        let needed =
            usize::try_from(entry.size.saturating_add(63) / 64).map_err(|_| EngineError::Unsupported("AAF mini-stream is too large".into()))?;
        if chain.len() != needed {
            return Err(EngineError::Unsupported(format!("AAF `{}` has a truncated mini-stream", label(&entry.name))));
        }
        let final_sector = chain.last().copied().unwrap_or(0);
        let end = u64::from(final_sector)
            .checked_add(1)
            .and_then(|sector| sector.checked_mul(64))
            .ok_or_else(|| EngineError::Unsupported("AAF mini-stream offset overflows".into()))?;
        if end > root_size {
            return Err(EngineError::Unsupported(format!("AAF `{}` mini-stream is outside the root stream", label(&entry.name))));
        }
    } else {
        validate_regular_stream(entry, fat, sector_count, &entry.name)?;
    }
    Ok(())
}

fn validate_regular_stream(entry: &DirectoryEntry, fat: &[u32], sector_count: usize, name: &str) -> Result<()> {
    if entry.size > MAX_AAF_METADATA_STREAM_BYTES {
        return Err(EngineError::Unsupported(format!(
            "AAF `{}` stream exceeds the {} MiB metadata limit",
            label(name),
            MAX_AAF_METADATA_STREAM_BYTES / (1024 * 1024)
        )));
    }
    if entry.size == 0 {
        return Ok(());
    }
    let chain = walk_chain(entry.start_sector, fat, sector_count, name)?;
    let needed = usize::try_from(entry.size.saturating_add(CFB_SECTOR_SIZE - 1) / CFB_SECTOR_SIZE)
        .map_err(|_| EngineError::Unsupported("AAF stream is too large".into()))?;
    if chain.len() != needed {
        return Err(EngineError::Unsupported(format!("AAF `{}` has a truncated stream", label(name))));
    }
    Ok(())
}

fn walk_chain(start: u32, table: &[u32], sector_count: usize, name: &str) -> Result<Vec<u32>> {
    if is_chain_end(start) {
        return Ok(Vec::new());
    }
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut sector = start;
    loop {
        valid_sector(sector, sector_count, name)?;
        if !seen.insert(sector) {
            return Err(EngineError::Unsupported(format!("AAF CFB {name} chain has a cycle")));
        }
        chain.push(sector);
        let index = usize::try_from(sector).map_err(|_| EngineError::Unsupported(format!("AAF CFB {name} index overflows")))?;
        let next =
            table.get(index).copied().ok_or_else(|| EngineError::Unsupported(format!("AAF CFB {name} chain is outside its allocation table")))?;
        if is_chain_end(next) {
            break;
        }
        sector = next;
    }
    Ok(chain)
}

fn valid_sector(sector: u32, sector_count: usize, name: &str) -> Result<()> {
    if is_chain_end(sector) || matches!(sector, CFB_FAT_SECTOR | CFB_DIFAT_SECTOR) {
        return Err(EngineError::Unsupported(format!("AAF CFB has an invalid {name} sector reference")));
    }
    let sector = usize::try_from(sector).map_err(|_| EngineError::Unsupported(format!("AAF CFB {name} sector overflows")))?;
    if sector >= sector_count {
        return Err(EngineError::Unsupported(format!("AAF CFB {name} sector is outside the file")));
    }
    Ok(())
}

fn is_chain_end(sector: u32) -> bool {
    matches!(sector, CFB_END_OF_CHAIN | CFB_FREE_SECTOR)
}

fn read_sector(file: &mut ConfinedHandle, sector: u32, input: &Path) -> Result<[u8; CFB_SECTOR_SIZE as usize]> {
    let offset = u64::from(sector)
        .checked_mul(CFB_SECTOR_SIZE)
        .and_then(|offset| offset.checked_add(CFB_SECTOR_SIZE))
        .ok_or_else(|| EngineError::Unsupported("AAF CFB sector offset overflows".into()))?;
    let mut bytes = [0u8; CFB_SECTOR_SIZE as usize];
    file.seek(SeekFrom::Start(offset)).map_err(|err| EngineError::Io(format!("{}: {err}", input.display())))?;
    // `preflight_cfb` sizes `sector_count` so this call is only ever made for a sector that
    // starts before EOF; a short read here can only be the final, partially-written sector a
    // real AAF writer left behind (see `preflight_cfb`), so the unfilled tail is left zeroed
    // instead of treated as an error. Reading zero bytes at all means `sector` named something
    // past EOF despite that guarantee, which is reported rather than silently accepted.
    let mut filled = 0usize;
    while filled < bytes.len() {
        let read = file.read(&mut bytes[filled..]).map_err(|err| EngineError::Io(format!("{}: {err}", input.display())))?;
        if read == 0 {
            break;
        }
        filled = filled.saturating_add(read);
    }
    if filled == 0 {
        return Err(EngineError::Unsupported(format!("{}: AAF CFB sector {sector} is past the end of the file", input.display())));
    }
    Ok(bytes)
}

fn parse_directory_entry(data: &[u8], offset: usize) -> Result<DirectoryEntry> {
    let entry =
        data.get(offset..offset.saturating_add(128)).ok_or_else(|| EngineError::Unsupported("AAF CFB directory entry is truncated".into()))?;
    let name_len = usize::from(le_u16(entry, 64)?).min(64);
    let name_end = name_len.saturating_sub(2).min(64);
    let mut code_units = Vec::new();
    for index in (0..name_end).step_by(2) {
        code_units.push(le_u16(entry, index)?);
    }
    Ok(DirectoryEntry {
        name: String::from_utf16_lossy(&code_units),
        kind: *entry.get(66).ok_or_else(|| EngineError::Unsupported("AAF CFB directory entry is truncated".into()))?,
        left: le_u32(entry, 68)?,
        right: le_u32(entry, 72)?,
        child: le_u32(entry, 76)?,
        start_sector: le_u32(entry, 116)?,
        size: le_u64(entry, 120)?,
    })
}

fn le_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let bytes: [u8; 2] = bytes
        .get(offset..offset.saturating_add(2))
        .ok_or_else(|| EngineError::Unsupported("AAF CFB field is truncated".into()))?
        .try_into()
        .map_err(|_| EngineError::Unsupported("AAF CFB field is truncated".into()))?;
    Ok(u16::from_le_bytes(bytes))
}

fn le_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let bytes: [u8; 4] = bytes
        .get(offset..offset.saturating_add(4))
        .ok_or_else(|| EngineError::Unsupported("AAF CFB field is truncated".into()))?
        .try_into()
        .map_err(|_| EngineError::Unsupported("AAF CFB field is truncated".into()))?;
    Ok(u32::from_le_bytes(bytes))
}

fn le_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let bytes: [u8; 8] = bytes
        .get(offset..offset.saturating_add(8))
        .ok_or_else(|| EngineError::Unsupported("AAF CFB field is truncated".into()))?
        .try_into()
        .map_err(|_| EngineError::Unsupported("AAF CFB field is truncated".into()))?;
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximedia_aaf::composition::{CompositionMob, Sequence, SequenceComponent, SourceClip, Track};
    use oximedia_aaf::dictionary::Auid;
    use oximedia_aaf::object_model::{Mob, MobType};
    use oximedia_aaf::timeline::{EditRate, Position};
    use std::fs::File;
    use std::io::Write;

    fn synthetic_aaf(path: &Path) {
        let source_id = uuid::Uuid::new_v4();
        let mut file = oximedia_aaf::AafFile::new();
        file.content_storage_mut().add_mob(Mob::new(source_id, "take.wav".into(), MobType::Source));
        let mut composition = CompositionMob::new(uuid::Uuid::new_v4(), "AAF timing");
        let mut sequence = Sequence::new(Auid::SOUND);
        sequence.add_component(SequenceComponent::Filler(oximedia_aaf::Filler::new(4_800)));
        sequence.add_component(SequenceComponent::SourceClip(SourceClip::new(48_000, Position::new(2_400), source_id, 1)));
        let mut track = Track::new(1, "DX", EditRate::AUDIO_48K, TrackType::Sound);
        track.origin = Position::new(9_600);
        track.set_sequence(sequence);
        composition.add_track(track);
        file.content_storage_mut().add_composition_mob(composition);
        let session = AafEditSession::from_file(file);
        let mut output = File::create(path).unwrap();
        session.write_to(&mut output).unwrap();
        output.flush().unwrap();
    }

    #[test]
    fn imports_timing_and_offline_wav_reference_from_a_synthetic_aaf() {
        let dir = crate::test_scratch::root().join(format!("soundcraft-aaf-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("timing.aaf");
        synthetic_aaf(&path);

        let mut engine = Engine::default();
        let report = import(&mut engine, &path, None).unwrap();

        assert_eq!(report.sample_rate, 48_000);
        assert_eq!(report.audio_tracks, 1);
        assert_eq!(report.clips, 1);
        assert_eq!(report.linked_wav_files, 0);
        let track = engine.session().tracks.first().unwrap();
        let clip = track.clips().first().unwrap();
        assert_eq!(track.name, "DX");
        assert_eq!(clip.start, 14_400);
        assert_eq!(clip.source_offset(), 2_400);
        assert_eq!(clip.length, 48_000);
        assert_eq!(engine.session().sources.first().map(|source| source.unsaved), Some(false));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn imports_a_cfb_container_with_trailing_slack_bytes_after_its_last_sector() {
        // Real Pro Tools AAF exports are commonly a few bytes short of the next sector boundary
        // (observed: a `The Gift.aaf` export landed 179 bytes into what would be its next
        // 512-byte sector). Those bytes sit past every sector `preflight_cfb` can address, so
        // they are inert slack rather than part of the container; such a file must still import.
        let dir = crate::test_scratch::root().join(format!("soundcraft-aaf-slack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("slack.aaf");
        synthetic_aaf(&path);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(&[0u8; 179]);
        std::fs::write(&path, &bytes).unwrap();

        let mut engine = Engine::default();
        let report = import(&mut engine, &path, None).unwrap();

        assert_eq!(report.audio_tracks, 1);
        assert_eq!(report.clips, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_a_non_compound_file_without_changing_the_document() {
        let dir = crate::test_scratch::root().join(format!("soundcraft-aaf-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.aaf");
        // Header sector plus one data sector of non-CFB bytes: long enough to pass the length
        // and sector-count preflight so the test exercises the magic-signature check (what a
        // `.ptx`/`.ptf` or other non-AAF file hits), not the "too short to be a container" check.
        std::fs::write(&path, [b'x'; 2 * CFB_SECTOR_SIZE as usize]).unwrap();

        let mut engine = Engine::default();
        engine.session_mut().name = "unchanged".into();
        let error = import(&mut engine, &path, None).unwrap_err();

        assert!(error.to_string().contains("compound-file"), "{error}");
        assert_eq!(engine.session().name, "unchanged");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_a_file_too_short_to_be_a_container_without_changing_the_document() {
        let dir = crate::test_scratch::root().join(format!("soundcraft-aaf-short-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("short.aaf");
        std::fs::write(&path, b"not an AAF").unwrap();

        let mut engine = Engine::default();
        engine.session_mut().name = "unchanged".into();
        let error = import(&mut engine, &path, None).unwrap_err();

        assert!(error.to_string().contains("CFB"), "{error}");
        assert_eq!(engine.session().name, "unchanged");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_an_aaf_larger_than_the_file_size_limit() {
        let dir = crate::test_scratch::root().join(format!("soundcraft-aaf-huge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("huge.aaf");
        // Sparse file: the limit is checked from metadata length before any read.
        let file = File::create(&path).unwrap();
        file.set_len(MAX_AAF_FILE_BYTES + 1).unwrap();
        drop(file);

        let mut engine = Engine::default();
        let error = import(&mut engine, &path, None).unwrap_err();

        assert!(error.to_string().contains("MiB"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn session_report_keeps_only_migration_relevant_lines() {
        let dir = crate::test_scratch::root().join(format!("soundcraft-aaf-report-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.txt");
        std::fs::write(
            &path,
            "SESSION NAME: The Gift\n\
             SAMPLE RATE: 48000\n\
             BIT DEPTH: 24-bit\n\
             # OF AUDIO TRACKS: 12\n\
             an unrelated line that should be dropped\n\
             TRACK NAME: Lead Vocal\n\
             PLUG-INS:  1 - Some AAX Plug-in\n\
             CHANNEL  EVENT  CLIP NAME  START TIME\n",
        )
        .unwrap();

        let notes = session_report_notes(&path).unwrap();

        assert!(notes.iter().any(|n| n.contains("SESSION NAME")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("SAMPLE RATE")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("BIT DEPTH")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("# OF AUDIO TRACKS")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("TRACK NAME")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("PLUG-INS")), "{notes:?}");
        assert!(!notes.iter().any(|n| n.contains("unrelated line")), "{notes:?}");
        assert!(!notes.iter().any(|n| n.contains("CHANNEL  EVENT")), "{notes:?}");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn session_report_notes_are_bounded_and_say_so() {
        let dir = crate::test_scratch::root().join(format!("soundcraft-aaf-report-bound-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.txt");
        let text = "PLUG-INS: filler\n".repeat(MAX_SESSION_REPORT_NOTES + 10);
        std::fs::write(&path, text).unwrap();

        let notes = session_report_notes(&path).unwrap();

        assert_eq!(notes.len(), MAX_SESSION_REPORT_NOTES);
        assert_eq!(notes.last().map(String::as_str), Some("further session-report notes were suppressed"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn session_report_notes_reports_a_missing_file_clearly() {
        let dir = crate::test_scratch::root().join(format!("soundcraft-aaf-report-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("missing.txt");

        let error = session_report_notes(&path).unwrap_err();

        assert!(error.to_string().contains("missing.txt"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }
}

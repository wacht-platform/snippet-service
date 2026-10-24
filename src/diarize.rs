use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use ort::session::Session;
use ort::value::Tensor;
use transcribe_rs::features::{MelConfig, compute_mel};

use crate::store::Store;

const RATE: usize = 16000;
const WINDOW: usize = 160_000;
const FRAMES: usize = 589;
const CLASSES: usize = 7;
const SHIFT: usize = 270;
const FIELD: usize = 991;
const MERGE_GAP: usize = RATE * 3 / 10;
const MIN_SEG: usize = RATE * 15 / 100;
const MIN_EMBED: usize = RATE / 2;
const MIN_CLEAN: usize = RATE;
const MIN_NEW: usize = RATE * 3 / 2;
const MAX_EMBED: usize = RATE * 15;
const MATCH: f32 = 0.40;
const NEW_BELOW: f32 = 0.30;
const SPEAKER_MERGE: f32 = 0.55;
const USER_MATCH: f32 = 0.30;
const USER_GUARD: f32 = 0.22;
const USER_FOLD: f32 = 0.40;
const USER_ADAPT: f32 = 0.55;
const ENROL_MIN: f32 = 12.0;
const ENROL_MAX: usize = RATE * 90;
const ENROL_CHUNK: usize = RATE * 8;

const MAP: [[bool; 3]; CLASSES] = [
    [false, false, false],
    [true, false, false],
    [false, true, false],
    [false, false, true],
    [true, true, false],
    [true, false, true],
    [false, true, true],
];

#[derive(Debug, Clone)]
pub struct Turn {
    pub speaker: i64,
    pub start: f32,
    pub end: f32,
}

#[derive(Debug, Clone)]
pub struct SpeakerInfo {
    pub id: i64,
    pub label: String,
    pub is_user: bool,
    pub lines: i64,
    pub last_seen: i64,
}

pub fn display_name(info: &SpeakerInfo) -> String {
    if info.is_user {
        "You".to_string()
    } else if info.label.is_empty() {
        format!("Speaker {}", info.id)
    } else {
        info.label.clone()
    }
}

struct Engine {
    seg: Session,
    emb: Session,
    used: Instant,
}

static ENGINE: OnceLock<Mutex<Option<Engine>>> = OnceLock::new();
static ASSIGN: Mutex<()> = Mutex::new(());

fn engine() -> &'static Mutex<Option<Engine>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

pub fn unload() {
    *engine().lock().unwrap() = None;
}

pub fn unload_idle(after: std::time::Duration) {
    let mut guard = engine().lock().unwrap();
    if guard.as_ref().is_some_and(|e| e.used.elapsed() > after) {
        *guard = None;
    }
}

fn open(path: std::path::PathBuf) -> Result<Session, String> {
    Session::builder()
        .map_err(|e| e.to_string())?
        .with_intra_threads(2)
        .map_err(|e| e.to_string())?
        .commit_from_file(path)
        .map_err(|e| e.to_string())
}

pub fn warm() -> Result<(), String> {
    let mut guard = engine().lock().unwrap();
    if guard.is_some() {
        return Ok(());
    }
    if !crate::models::is_ready(crate::models::SPEAKER_MODEL) {
        return Err("the speaker model is not downloaded".into());
    }
    let dir = crate::models::dir(crate::models::SPEAKER_MODEL);
    *guard = Some(Engine {
        seg: open(dir.join("segmentation.onnx"))?,
        emb: open(dir.join("embedding.onnx"))?,
        used: Instant::now(),
    });
    Ok(())
}

pub fn read_audio_16k(path: &std::path::Path) -> Result<Vec<f32>, String> {
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-ar", "16000", "-ac", "1", "-f", "f32le", "-"])
        .output()
        .map_err(|e| format!("could not read audio: {e}"))?;
    if !out.status.success() {
        return Err(format!("could not read audio: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(out
        .stdout
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}

struct Window {
    start: usize,
    labels: Vec<[bool; 3]>,
}

fn run_segmentation(eng: &mut Engine, samples: &[f32]) -> Result<Vec<Window>, String> {
    let name = eng.seg.inputs()[0].name().to_string();
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < samples.len() {
        let chunk = &samples[start..(start + WINDOW).min(samples.len())];
        if chunk.len() >= FIELD {
            let mut x = vec![0f32; WINDOW];
            x[..chunk.len()].copy_from_slice(chunk);
            let outputs = eng
                .seg
                .run(ort::inputs![name.as_str() => Tensor::from_array(([1usize, 1usize, WINDOW], x)).map_err(|e| e.to_string())?])
                .map_err(|e| e.to_string())?;
            let (_, y) = outputs[0].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
            let valid = ((chunk.len() - FIELD) / SHIFT + 1).min(FRAMES);
            let mut labels = Vec::with_capacity(valid);
            for f in 0..valid {
                let row = &y[f * CLASSES..(f + 1) * CLASSES];
                let best = row
                    .iter()
                    .enumerate()
                    .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                labels.push(MAP[best]);
            }
            out.push(Window { start, labels });
        }
        start += WINDOW;
    }
    Ok(out)
}

fn ranges(w: &Window, k: usize, clean: bool, len: usize) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut open: Option<usize> = None;
    let flush = |runs: &mut Vec<(usize, usize)>, from: usize, to: usize| {
        let a = w.start + from * SHIFT;
        let b = (w.start + to * SHIFT + SHIFT).min(len);
        if b > a {
            runs.push((a, b));
        }
    };
    for (f, l) in w.labels.iter().enumerate() {
        let on = l[k] && (!clean || l.iter().filter(|x| **x).count() == 1);
        match (on, open) {
            (true, None) => open = Some(f),
            (false, Some(s)) => {
                flush(&mut runs, s, f - 1);
                open = None;
            }
            _ => {}
        }
    }
    if let Some(s) = open {
        flush(&mut runs, s, w.labels.len() - 1);
    }
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for r in runs {
        match merged.last_mut() {
            Some(last) if r.0 <= last.1 + MERGE_GAP => last.1 = r.1.max(last.1),
            _ => merged.push(r),
        }
    }
    merged.retain(|(a, b)| b - a >= MIN_SEG);
    merged
}

fn gather(samples: &[f32], rs: &[(usize, usize)], cap: usize) -> Vec<f32> {
    let mut out = Vec::new();
    for (a, b) in rs {
        out.extend_from_slice(&samples[*a..(*b).min(samples.len())]);
        if out.len() >= cap {
            out.truncate(cap);
            break;
        }
    }
    out
}

fn normalise(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

fn embed(eng: &mut Engine, audio: &[f32]) -> Result<Vec<f32>, String> {
    let cfg = MelConfig { normalize_samples: false, ..Default::default() };
    let f = compute_mel(audio, &cfg);
    let (t, d) = f.dim();
    if t < 20 {
        return Err("audio is too short".into());
    }
    let mut data: Vec<f32> = f.iter().copied().collect();
    for c in 0..d {
        let mean: f32 = (0..t).map(|r| data[r * d + c]).sum::<f32>() / t as f32;
        for r in 0..t {
            data[r * d + c] -= mean;
        }
    }
    let name = eng.emb.inputs()[0].name().to_string();
    let outputs = eng
        .emb
        .run(ort::inputs![name.as_str() => Tensor::from_array(([1usize, t, d], data)).map_err(|e| e.to_string())?])
        .map_err(|e| e.to_string())?;
    let (_, e) = outputs[0].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
    let mut v = e.to_vec();
    normalise(&mut v);
    Ok(v)
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

struct Sp {
    id: i64,
    is_user: bool,
    lines: i64,
    centroid: Vec<f32>,
}

fn blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn unblob(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

fn load_speakers(store: &Store) -> Result<Vec<Sp>, String> {
    store
        .with_connection(|c| {
            let mut stmt = c.prepare("SELECT id, is_user, lines, centroid FROM speakers ORDER BY id")?;
            let rows = stmt.query_map([], |r| {
                Ok(Sp {
                    id: r.get(0)?,
                    is_user: r.get::<_, i64>(1)? != 0,
                    lines: r.get(2)?,
                    centroid: unblob(&r.get::<_, Vec<u8>>(3)?),
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .map_err(|e| e.to_string())
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

struct Item {
    window: usize,
    segs: Vec<(usize, usize)>,
    emb: Option<Vec<f32>>,
    secs: usize,
}

fn assign(store: &Store, items: &[Item]) -> Result<Vec<i64>, String> {
    let _guard = ASSIGN.lock().unwrap();
    let mut sps = load_speakers(store)?;
    if let Some(user) = sps.iter().find(|s| s.is_user).map(|s| s.centroid.clone()) {
        let dupes: Vec<i64> = sps
            .iter()
            .filter(|s| !s.is_user && s.centroid.len() == user.len() && dot(&s.centroid, &user) >= USER_FOLD)
            .map(|s| s.id)
            .collect();
        for id in &dupes {
            let _ = store.with_connection(|c| c.execute("DELETE FROM speakers WHERE id = ?1", [id]));
        }
        sps.retain(|s| !dupes.contains(&s.id));
    }
    let mut result = vec![-1i64; items.len()];
    let mut done = vec![false; items.len()];
    let mut used: HashSet<(usize, i64)> = HashSet::new();
    let mut touched: HashSet<i64> = HashSet::new();
    let mut cands: Vec<(f32, usize, usize)> = Vec::new();
    for (i, it) in items.iter().enumerate() {
        if let Some(e) = &it.emb {
            for (s, sp) in sps.iter().enumerate() {
                if sp.centroid.len() == e.len() {
                    cands.push((dot(e, &sp.centroid), i, s));
                }
            }
        }
    }
    cands.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    for (sim, i, s) in &cands {
        if done[*i] || *sim < if sps[*s].is_user { USER_MATCH } else { MATCH } || (!sps[*s].is_user && used.contains(&(items[*i].window, sps[*s].id))) {
            continue;
        }
        done[*i] = true;
        used.insert((items[*i].window, sps[*s].id));
        result[*i] = sps[*s].id;
        touched.insert(sps[*s].id);
        if sps[*s].is_user {
            if *sim >= USER_ADAPT {
                let e = items[*i].emb.as_ref().unwrap();
                let mut c: Vec<f32> = sps[*s].centroid.iter().zip(e).map(|(a, b)| a * 0.97 + b * 0.03).collect();
                normalise(&mut c);
                sps[*s].centroid = c;
            }
        } else {
            let e = items[*i].emb.as_ref().unwrap();
            let w = sps[*s].lines.min(30) as f32;
            let mut c: Vec<f32> = sps[*s].centroid.iter().zip(e).map(|(a, b)| a * w + b).collect();
            normalise(&mut c);
            sps[*s].centroid = c;
        }
        sps[*s].lines += 1;
    }
    for i in 0..items.len() {
        if done[i] || items[i].emb.is_none() {
            continue;
        }
        let e = items[i].emb.as_ref().unwrap();
        let best = sps
            .iter()
            .enumerate()
            .filter(|(_, sp)| (sp.is_user || !used.contains(&(items[i].window, sp.id))) && sp.centroid.len() == e.len())
            .map(|(s, sp)| (dot(e, &sp.centroid), s))
            .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        match best {
            Some((sim, s)) if sim >= NEW_BELOW => {
                used.insert((items[i].window, sps[s].id));
                result[i] = sps[s].id;
                touched.insert(sps[s].id);
                sps[s].lines += 1;
            }
            _ if items[i].secs >= MIN_NEW
                && sps.iter().any(|sp| {
                    sp.is_user && sp.centroid.len() == e.len() && dot(e, &sp.centroid) >= USER_GUARD
                }) =>
            {
                if let Some(sp) = sps.iter_mut().find(|sp| sp.is_user) {
                    used.insert((items[i].window, sp.id));
                    result[i] = sp.id;
                    touched.insert(sp.id);
                    sp.lines += 1;
                }
            }
            _ if items[i].secs >= MIN_NEW => {
                let at = now();
                let id = store
                    .with_connection(|c| {
                        c.execute(
                            "INSERT INTO speakers (label, is_user, centroid, lines, last_seen) VALUES ('', 0, ?1, 1, ?2)",
                            rusqlite::params![blob(e), at],
                        )?;
                        Ok(c.last_insert_rowid())
                    })
                    .map_err(|e| e.to_string())?;
                used.insert((items[i].window, id));
                result[i] = id;
                sps.push(Sp { id, is_user: false, lines: 1, centroid: e.clone() });
            }
            _ => {}
        }
    }
    loop {
        let mut pair = None;
        'scan: for i in 0..sps.len() {
            for j in (i + 1)..sps.len() {
                if !sps[i].is_user && !sps[j].is_user && sps[i].centroid.len() == sps[j].centroid.len() && dot(&sps[i].centroid, &sps[j].centroid) >= SPEAKER_MERGE {
                    pair = Some(if sps[i].lines >= sps[j].lines { (i, j) } else { (j, i) });
                    break 'scan;
                }
            }
        }
        let Some((keep, drop)) = pair else { break };
        let gone = sps.remove(drop);
        let keep = if drop < keep { keep - 1 } else { keep };
        let (kw, gw) = (sps[keep].lines.min(60) as f32, gone.lines.min(60) as f32);
        let mut c: Vec<f32> = sps[keep].centroid.iter().zip(&gone.centroid).map(|(a, b)| a * kw + b * gw).collect();
        normalise(&mut c);
        sps[keep].centroid = c;
        sps[keep].lines += gone.lines;
        let kid = sps[keep].id;
        for r in result.iter_mut().filter(|r| **r == gone.id) {
            *r = kid;
        }
        touched.remove(&gone.id);
        touched.insert(kid);
        let _ = store.with_connection(|c| {
            c.execute("DELETE FROM speakers WHERE id = ?1", [gone.id])?;
            Ok(())
        });
    }
    let at = now();
    for sp in sps.iter().filter(|s| touched.contains(&s.id)) {
        store
            .with_connection(|c| {
                c.execute(
                    "UPDATE speakers SET centroid = ?1, lines = ?2, last_seen = ?3 WHERE id = ?4",
                    rusqlite::params![blob(&sp.centroid), sp.lines, at, sp.id],
                )
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(result)
}

pub fn user_similarity(store: &Store, samples: &[f32]) -> Result<Option<f32>, String> {
    let Some(user) = load_speakers(store)?.into_iter().find(|s| s.is_user) else {
        return Ok(None);
    };
    if samples.len() < MIN_EMBED {
        return Ok(Some(0.0));
    }
    warm()?;
    let mut guard = engine().lock().unwrap();
    let eng = guard.as_mut().ok_or("speaker model is not loaded")?;
    eng.used = Instant::now();
    let audio = &samples[..samples.len().min(MAX_EMBED)];
    let e = embed(eng, audio)?;
    Ok(Some(if e.len() == user.centroid.len() { dot(&e, &user.centroid) } else { 0.0 }))
}

pub fn analyze(store: &Store, samples: &[f32]) -> Result<Vec<Turn>, String> {
    if samples.len() < MIN_EMBED {
        return Ok(Vec::new());
    }
    warm()?;
    let mut guard = engine().lock().unwrap();
    let eng = guard.as_mut().ok_or("speaker model is not loaded")?;
    eng.used = Instant::now();
    let windows = run_segmentation(eng, samples)?;
    let mut items: Vec<Item> = Vec::new();
    for (wi, w) in windows.iter().enumerate() {
        for k in 0..3 {
            let segs = ranges(w, k, false, samples.len());
            if segs.is_empty() {
                continue;
            }
            let clean = ranges(w, k, true, samples.len());
            let clean_len: usize = clean.iter().map(|(a, b)| b - a).sum();
            let audio = if clean_len >= MIN_CLEAN {
                gather(samples, &clean, MAX_EMBED)
            } else {
                gather(samples, &segs, MAX_EMBED)
            };
            let emb = if audio.len() >= MIN_EMBED { embed(eng, &audio).ok() } else { None };
            items.push(Item { window: wi, segs, emb, secs: audio.len() });
        }
    }
    drop(guard);
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let ids = assign(store, &items)?;
    let mut turns: Vec<Turn> = Vec::new();
    for (it, id) in items.iter().zip(ids) {
        for (a, b) in &it.segs {
            turns.push(Turn { speaker: id, start: *a as f32 / RATE as f32, end: *b as f32 / RATE as f32 });
        }
    }
    turns.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));
    let known: Vec<Turn> = turns.iter().filter(|t| t.speaker != -1).cloned().collect();
    for t in turns.iter_mut().filter(|t| t.speaker == -1 && t.end - t.start < 1.0) {
        let near = known.iter().min_by(|a, b| {
            let gap = |x: &Turn| if t.start > x.end { t.start - x.end } else if x.start > t.end { x.start - t.end } else { 0.0 };
            gap(a).partial_cmp(&gap(b)).unwrap_or(std::cmp::Ordering::Equal)
        });
        if let Some(n) = near {
            t.speaker = n.speaker;
        }
    }
    if let Some(user) = speakers(store).into_iter().find(|s| s.is_user).map(|s| s.id) {
        let snapshot = turns.clone();
        for t in turns.iter_mut().filter(|t| t.speaker != user && t.end - t.start < 1.0) {
            let touching = snapshot.iter().any(|x| {
                x.speaker == user && (if t.start > x.end { t.start - x.end } else if x.start > t.end { x.start - t.end } else { 0.0 }) < 0.4
            });
            if touching {
                t.speaker = user;
            }
        }
    }
    let mut merged: Vec<Turn> = Vec::new();
    for t in turns {
        match merged.last_mut() {
            Some(last) if last.speaker == t.speaker && t.start - last.end < MERGE_GAP as f32 / RATE as f32 => {
                last.end = last.end.max(t.end)
            }
            _ => merged.push(t),
        }
    }
    Ok(merged)
}

pub fn enroll(store: &Store, samples: &[f32]) -> Result<f32, String> {
    warm()?;
    let mut guard = engine().lock().unwrap();
    let eng = guard.as_mut().ok_or("speaker model is not loaded")?;
    eng.used = Instant::now();
    let windows = run_segmentation(eng, samples)?;
    let mut speech: Vec<f32> = Vec::new();
    for w in &windows {
        for k in 0..3 {
            let rs = ranges(w, k, true, samples.len());
            speech.extend(gather(samples, &rs, usize::MAX));
        }
    }
    speech.truncate(ENROL_MAX);
    let seconds = speech.len() as f32 / RATE as f32;
    if seconds < ENROL_MIN {
        return Err(format!(
            "Only {seconds:.0} seconds of clear speech found. Record at least 20 seconds, speaking steadily with nobody else talking."
        ));
    }
    let mut parts: Vec<Vec<f32>> = Vec::new();
    for chunk in speech.chunks(ENROL_CHUNK) {
        if chunk.len() >= RATE * 3 {
            parts.push(embed(eng, chunk)?);
        }
    }
    drop(guard);
    if parts.is_empty() {
        return Err("The recording was too short to use.".into());
    }
    if parts.len() > 1 {
        let mut total = 0.0f32;
        let mut n = 0;
        for i in 0..parts.len() {
            for j in i + 1..parts.len() {
                total += dot(&parts[i], &parts[j]);
                n += 1;
            }
        }
        if total / (n as f32) < 0.25 {
            return Err("The recording sounds like more than one voice, or too much noise. Try again somewhere quiet.".into());
        }
    }
    let mut centroid = vec![0f32; parts[0].len()];
    for p in &parts {
        for (c, v) in centroid.iter_mut().zip(p) {
            *c += v;
        }
    }
    normalise(&mut centroid);
    let _guard = ASSIGN.lock().unwrap();
    let at = now();
    store
        .with_connection(|c| {
            c.execute("DELETE FROM speakers WHERE is_user = 1", [])?;
            let mut stmt = c.prepare("SELECT id, centroid FROM speakers")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, unblob(&r.get::<_, Vec<u8>>(1)?))))?
                .collect::<Result<Vec<_>, _>>()?;
            for (id, cen) in rows {
                if cen.len() == centroid.len() && dot(&cen, &centroid) > 0.6 {
                    c.execute("DELETE FROM speakers WHERE id = ?1", [id])?;
                }
            }
            c.execute(
                "INSERT INTO speakers (label, is_user, centroid, lines, last_seen) VALUES ('', 1, ?1, 0, ?2)",
                rusqlite::params![blob(&centroid), at],
            )?;
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    Ok(seconds)
}

pub fn unenroll(store: &Store) -> Result<(), String> {
    let _guard = ASSIGN.lock().unwrap();
    store
        .with_connection(|c| c.execute("DELETE FROM speakers WHERE is_user = 1", []).map(|_| ()))
        .map_err(|e| e.to_string())
}

pub fn clear(store: &Store) -> Result<(), String> {
    let _guard = ASSIGN.lock().unwrap();
    store
        .with_connection(|c| c.execute("DELETE FROM speakers WHERE is_user = 0", []).map(|_| ()))
        .map_err(|e| e.to_string())
}

pub fn rename(store: &Store, id: i64, label: &str) -> Result<(), String> {
    let label = label.trim().chars().take(40).collect::<String>();
    store
        .with_connection(|c| {
            c.execute("UPDATE speakers SET label = ?1 WHERE id = ?2 AND is_user = 0", rusqlite::params![label, id]).map(|_| ())
        })
        .map_err(|e| e.to_string())
}

pub fn speakers(store: &Store) -> Vec<SpeakerInfo> {
    store
        .with_connection(|c| {
            let mut stmt = c.prepare(
                "SELECT id, label, is_user, lines, last_seen FROM speakers ORDER BY is_user DESC, last_seen DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(SpeakerInfo {
                    id: r.get(0)?,
                    label: r.get(1)?,
                    is_user: r.get::<_, i64>(2)? != 0,
                    lines: r.get(3)?,
                    last_seen: r.get(4)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_default()
}

pub fn enrolled(store: &Store) -> bool {
    store
        .with_connection(|c| c.query_row("SELECT COUNT(*) FROM speakers WHERE is_user = 1", [], |r| r.get::<_, i64>(0)))
        .map(|n| n > 0)
        .unwrap_or(false)
}

pub fn claim(store: &Store, id: i64, explicit: bool) -> Result<(), String> {
    let _guard = ASSIGN.lock().unwrap();
    store
        .with_connection(|c| {
            let user: i64 = c.query_row("SELECT COUNT(*) FROM speakers WHERE is_user = 1", [], |r| r.get(0))?;
            if user > 0 && !explicit {
                return Ok(());
            }
            let exists: i64 = c.query_row("SELECT COUNT(*) FROM speakers WHERE id = ?1", [id], |r| r.get(0))?;
            if exists == 0 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            if explicit {
                c.execute("UPDATE speakers SET is_user = 0 WHERE is_user = 1 AND id != ?1", [id])?;
            }
            c.execute("UPDATE speakers SET is_user = 1, label = '' WHERE id = ?1", [id]).map(|_| ())
        })
        .map_err(|e| { let m = e.to_string(); if m.contains("no rows") { "no such speaker".to_string() } else { m } })
}

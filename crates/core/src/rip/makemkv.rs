//! Driving MakeMKV, and reading its robot output.
//!
//! `makemkvcon -r` emits one record per line, `TYPE:field,field,...`, with
//! quoted strings that may contain commas. It is far more parseable than the
//! human output, and it is the reason a disc can be enumerated - titles,
//! durations, chapter counts, every track and its language - without ripping
//! anything. That enumeration is what lets us identify a disc *before*
//! committing forty minutes to reading it.

use crate::host::{Command, Runner};
use crate::model::{DiscScan, DiscTitle, Drive, Millis, Track, TrackKind};
use crate::{Error, Result};

/// MakeMKV's attribute ids, from `AP_ItemAttributeId`. Only the ones we read.
mod attr {
    pub const TYPE: u32 = 1;
    pub const NAME: u32 = 2;
    pub const LANG_CODE: u32 = 3;
    pub const CODEC_SHORT: u32 = 6;
    pub const CHAPTER_COUNT: u32 = 8;
    pub const DURATION: u32 = 9;
    pub const DISK_SIZE_BYTES: u32 = 11;
    pub const AUDIO_CHANNELS: u32 = 14;
    pub const OUTPUT_FILE_NAME: u32 = 27;
}

/// Split a robot-output record into its fields.
///
/// Quoted fields may contain commas and escaped quotes, so this cannot be a
/// `split(',')` - a track named `Commentary, with cast` would silently shift
/// every field after it.
pub fn split_fields(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// `1:23:45` or `23:45` to milliseconds.
pub fn parse_duration(s: &str) -> Millis {
    let parts: Vec<u64> =
        s.trim().split(':').map(|p| p.trim().parse::<u64>().unwrap_or(0)).collect();
    let secs = match parts.len() {
        3 => parts[0] * 3600 + parts[1] * 60 + parts[2],
        2 => parts[0] * 60 + parts[1],
        1 => parts[0],
        _ => 0,
    };
    secs * 1000
}

fn kind_of(type_name: &str) -> TrackKind {
    match type_name.trim().to_ascii_lowercase().as_str() {
        "video" => TrackKind::Video,
        "audio" => TrackKind::Audio,
        // MakeMKV says "Subtitles", ffmpeg says "subtitle"
        "subtitles" | "subtitle" => TrackKind::Subtitle,
        _ => TrackKind::Other,
    }
}

/// Parse the `DRV:` records from `makemkvcon -r info`.
pub fn parse_drives(output: &str) -> Vec<Drive> {
    let mut out = Vec::new();
    for line in output.lines() {
        let Some(rest) = line.strip_prefix("DRV:") else { continue };
        let f = split_fields(rest);
        if f.len() < 7 {
            continue;
        }
        // DRV:index,visible,enabled,flags,name,disc label,device
        let label = f[5].trim().to_string();
        let device = f[6].trim().to_string();
        // An empty device is a drive slot MakeMKV lists but cannot use.
        if device.is_empty() {
            continue;
        }
        out.push(Drive {
            id: format!("disc:{}", f[0].trim()),
            device,
            name: f[4].trim().to_string(),
            disc_label: if label.is_empty() { None } else { Some(label) },
            kind: None,
        });
    }
    out
}

/// Parse the title and stream records into a scan.
pub fn parse_scan(output: &str, drive: Drive) -> Result<DiscScan> {
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Building {
        duration: Millis,
        chapters: usize,
        size: u64,
        output_name: String,
        streams: BTreeMap<u32, BTreeMap<u32, String>>,
    }

    let mut titles: BTreeMap<u32, Building> = BTreeMap::new();
    let mut label = drive.disc_label.clone().unwrap_or_default();

    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("CINFO:") {
            let f = split_fields(rest);
            // CINFO:id,code,value - attribute 2 (name) or 32 (volume name)
            if f.len() >= 3 {
                let id: u32 = f[0].trim().parse().unwrap_or(0);
                if (id == 32 || (id == attr::NAME && label.is_empty())) && !f[2].trim().is_empty() {
                    label = f[2].trim().to_string();
                }
            }
        } else if let Some(rest) = line.strip_prefix("TINFO:") {
            let f = split_fields(rest);
            if f.len() < 4 {
                continue;
            }
            let (Ok(title), Ok(id)) = (f[0].trim().parse::<u32>(), f[1].trim().parse::<u32>())
            else {
                continue;
            };
            let t = titles.entry(title).or_default();
            let v = f[3].trim();
            match id {
                attr::DURATION => t.duration = parse_duration(v),
                attr::CHAPTER_COUNT => t.chapters = v.parse().unwrap_or(0),
                attr::DISK_SIZE_BYTES => t.size = v.parse().unwrap_or(0),
                attr::OUTPUT_FILE_NAME => t.output_name = v.to_string(),
                _ => {}
            }
        } else if let Some(rest) = line.strip_prefix("SINFO:") {
            let f = split_fields(rest);
            if f.len() < 5 {
                continue;
            }
            let (Ok(title), Ok(stream), Ok(id)) = (
                f[0].trim().parse::<u32>(),
                f[1].trim().parse::<u32>(),
                f[2].trim().parse::<u32>(),
            ) else {
                continue;
            };
            titles
                .entry(title)
                .or_default()
                .streams
                .entry(stream)
                .or_default()
                .insert(id, f[4].trim().to_string());
        }
    }

    if titles.is_empty() {
        return Err(Error("MakeMKV found no titles - is there a disc in the drive?".into()));
    }

    let mut out = Vec::new();
    for (id, b) in titles {
        // Number each stream type from zero independently, matching how ffmpeg
        // will address them once ripped.
        let mut per_kind = [0usize; 4];
        let mut tracks = Vec::new();
        for props in b.streams.values() {
            let kind = kind_of(props.get(&attr::TYPE).map(String::as_str).unwrap_or(""));
            let slot = match kind {
                TrackKind::Video => 0,
                TrackKind::Audio => 1,
                TrackKind::Subtitle => 2,
                TrackKind::Other => 3,
            };
            let index = per_kind[slot];
            per_kind[slot] += 1;
            tracks.push(Track {
                kind,
                index,
                codec: props.get(&attr::CODEC_SHORT).cloned().unwrap_or_default(),
                language: props
                    .get(&attr::LANG_CODE)
                    .filter(|l| !l.is_empty())
                    .cloned()
                    .unwrap_or_else(|| "und".into()),
                channels: props
                    .get(&attr::AUDIO_CHANNELS)
                    .and_then(|c| c.parse().ok())
                    .unwrap_or(0),
                title: props.get(&attr::NAME).filter(|n| !n.is_empty()).cloned(),
                // Neither is known from a scan. What the ripped file says is
                // what counts, and it is probed before anything is decided.
                default: false,
                forced: false,
            });
        }
        out.push(DiscTitle {
            id,
            duration: b.duration,
            chapter_count: b.chapters,
            // MakeMKV reports a count but not the durations; those only arrive
            // once the title has been ripped and probed.
            chapters: Vec::new(),
            size_bytes: b.size,
            output_name: if b.output_name.is_empty() {
                format!("title_t{id:02}.mkv")
            } else {
                b.output_name
            },
            tracks,
        });
    }

    Ok(DiscScan { drive, label, titles: out })
}

/// List drives and whatever is loaded in them.
pub fn drives_command() -> Command {
    Command::new("makemkvcon").args(["-r", "--cache=1", "info", "disc:9999"])
}

/// Enumerate a disc without ripping it.
///
/// `--minlength` is in seconds and defaults low, because a two-minute title can
/// still be an extra worth keeping, and a title MakeMKV never lists is one we
/// cannot later decide about.
pub fn scan_command(drive: &str, min_length_seconds: u32) -> Command {
    Command::new("makemkvcon").args([
        "-r",
        "--cache=1",
        &format!("--minlength={min_length_seconds}"),
        "info",
        drive,
    ])
}

/// Rip specific titles, or all of them.
pub fn rip_command(
    drive: &str,
    title: Option<u32>,
    dest: &std::path::Path,
    min_length_seconds: u32,
) -> Command {
    Command::new("makemkvcon")
        .args([
            "-r",
            "--progress=-same",
            &format!("--minlength={min_length_seconds}"),
            "mkv",
            drive,
            &title.map(|t| t.to_string()).unwrap_or_else(|| "all".into()),
        ])
        .path(dest)
}

/// A progress report parsed out of `PRGV:` / `PRGC:`.
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    /// 0.0 to 1.0 for the whole operation.
    pub total: f32,
    /// What MakeMKV says it is doing.
    pub message: Option<String>,
}

/// Parse one line of rip output into progress, if it carries any.
pub fn parse_progress(line: &str) -> Option<Progress> {
    if let Some(rest) = line.strip_prefix("PRGV:") {
        let f = split_fields(rest);
        // PRGV:current,total,max
        let (Ok(total), Ok(max)) =
            (f.get(1)?.trim().parse::<f32>(), f.get(2)?.trim().parse::<f32>())
        else {
            return None;
        };
        if max <= 0.0 {
            return None;
        }
        return Some(Progress { total: (total / max).clamp(0.0, 1.0), message: None });
    }
    if let Some(rest) = line.strip_prefix("PRGT:") {
        // PRGT:code,id,name - the name of the current operation
        let f = split_fields(rest);
        return Some(Progress {
            total: f32::NAN,
            message: f.get(2).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
        });
    }
    None
}

/// Pull a human-readable failure out of the `MSG:` records.
pub fn parse_error(output: &str) -> Option<String> {
    for line in output.lines() {
        let Some(rest) = line.strip_prefix("MSG:") else { continue };
        let f = split_fields(rest);
        // MSG:code,flags,count,message,format,params...
        let Some(text) = f.get(3) else { continue };
        let lower = text.to_ascii_lowercase();
        if lower.contains("fail")
            || lower.contains("error")
            || lower.contains("cannot")
            || lower.contains("no disc")
        {
            return Some(text.trim().to_string());
        }
    }
    None
}

/// Did the run report a clean finish?
///
/// MakeMKV exits zero even when it saved fewer titles than it was asked for, so
/// the exit status alone is not enough.
pub fn saved_titles(output: &str) -> Option<(u32, u32)> {
    for line in output.lines() {
        let Some(rest) = line.strip_prefix("MSG:5036,") else { continue };
        let f = split_fields(rest);
        // flags,count,message,format,param1,param2 - the counts are the params
        let nums: Vec<u32> =
            f.iter().skip(4).filter_map(|s| s.trim().parse::<u32>().ok()).collect();
        if nums.len() >= 2 {
            return Some((nums[0], nums[1]));
        }
    }
    None
}

/// Check for the tool being present at all, with a message that says what to do.
pub fn ensure_available(runner: &dyn Runner) -> Result<()> {
    match runner.run(&Command::new("makemkvcon").arg("-r").arg("--version")) {
        Ok(_) => Ok(()),
        Err(_) => Err(Error("makemkvcon not found - install MakeMKV to rip discs".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = r#"MSG:1005,0,1,"MakeMKV v1.17.8 started","%1 started","MakeMKV v1.17.8"
DRV:0,2,999,12,"BD-RE HL-DT-ST BD-RE WH16NS40 1.05","PARKS_AND_RECREATION_S7D1","/dev/sr0"
DRV:1,256,999,0,"","",""
TCOUNT:3
CINFO:32,0,"PARKS_AND_RECREATION_S7D1"
TINFO:0,2,0,"Parks and Recreation"
TINFO:0,8,0,"6"
TINFO:0,9,0,"0:21:15"
TINFO:0,11,0,"1503238553"
TINFO:0,27,0,"title_t00.mkv"
SINFO:0,0,1,6201,"Video"
SINFO:0,0,6,0,"Mpeg2"
SINFO:0,1,1,6202,"Audio"
SINFO:0,1,3,0,"eng"
SINFO:0,1,6,0,"DD"
SINFO:0,1,14,0,"6"
SINFO:0,2,1,6202,"Audio"
SINFO:0,2,2,0,"Commentary, with the cast"
SINFO:0,2,3,0,"eng"
SINFO:0,2,14,0,"2"
SINFO:0,3,1,6203,"Subtitles"
SINFO:0,3,3,0,"eng"
SINFO:0,3,6,0,"PGS"
TINFO:1,8,0,"24"
TINFO:1,9,0,"1:24:58"
TINFO:1,27,0,"title_t01.mkv"
TINFO:2,8,0,"2"
TINFO:2,9,0,"2:30"
TINFO:2,27,0,"title_t02.mkv"
"#;

    fn drive() -> Drive {
        parse_drives(INFO).into_iter().next().unwrap()
    }

    #[test]
    fn drives_come_out_with_their_disc_label() {
        let d = drive();
        assert_eq!(d.id, "disc:0");
        assert_eq!(d.device, "/dev/sr0");
        assert_eq!(d.disc_label.as_deref(), Some("PARKS_AND_RECREATION_S7D1"));
        assert!(d.has_disc());
    }

    #[test]
    fn empty_drive_slots_are_not_listed() {
        // MakeMKV pads its drive list with unusable entries
        assert_eq!(parse_drives(INFO).len(), 1);
    }

    #[test]
    fn a_drive_with_no_disc_is_listed_but_flagged_empty() {
        let d = parse_drives(r#"DRV:0,2,999,12,"Some Drive","","/dev/sr0""#);
        assert_eq!(d.len(), 1);
        assert!(!d[0].has_disc());
    }

    #[test]
    fn a_comma_inside_a_quoted_field_does_not_shift_the_others() {
        // "Commentary, with the cast" would otherwise push the language out of
        // its column and silently mislabel the track
        let s = parse_scan(INFO, drive()).unwrap();
        let audio = &s.titles[0].tracks;
        let commentary = audio.iter().find(|t| t.index == 1 && t.kind == TrackKind::Audio).unwrap();
        assert_eq!(commentary.title.as_deref(), Some("Commentary, with the cast"));
        assert_eq!(commentary.language, "eng");
        assert!(commentary.is_commentary());
    }

    #[test]
    fn escaped_quotes_survive() {
        let f = split_fields(r#"0,1,"say ""hi"" now",2"#);
        assert_eq!(f[2], r#"say "hi" now"#);
    }

    #[test]
    fn durations_parse_in_both_shapes() {
        assert_eq!(parse_duration("0:21:15"), 1_275_000);
        assert_eq!(parse_duration("1:24:58"), 5_098_000);
        assert_eq!(parse_duration("2:30"), 150_000);
        assert_eq!(parse_duration(""), 0);
    }

    #[test]
    fn titles_carry_everything_identification_needs() {
        let s = parse_scan(INFO, drive()).unwrap();
        assert_eq!(s.label, "PARKS_AND_RECREATION_S7D1");
        assert_eq!(s.titles.len(), 3);
        let t = &s.titles[0];
        assert_eq!(t.duration, 1_275_000);
        assert_eq!(t.chapter_count, 6);
        assert_eq!(t.size_bytes, 1_503_238_553);
        assert_eq!(t.output_name, "title_t00.mkv");
    }

    #[test]
    fn stream_indices_are_numbered_per_type_as_ffmpeg_will_see_them() {
        let s = parse_scan(INFO, drive()).unwrap();
        let t = &s.titles[0];
        let audio: Vec<usize> =
            t.tracks.iter().filter(|x| x.kind == TrackKind::Audio).map(|x| x.index).collect();
        assert_eq!(audio, vec![0, 1]);
        let subs: Vec<usize> =
            t.tracks.iter().filter(|x| x.kind == TrackKind::Subtitle).map(|x| x.index).collect();
        assert_eq!(subs, vec![0]);
    }

    #[test]
    fn makemkv_subtitles_map_onto_the_ffmpeg_spelling() {
        // "Subtitles" from one tool, "subtitle" from the other
        let s = parse_scan(INFO, drive()).unwrap();
        assert!(s.titles[0].tracks.iter().any(|t| t.kind == TrackKind::Subtitle));
    }

    #[test]
    fn a_missing_output_name_is_synthesised() {
        let s = parse_scan("TINFO:7,9,0,\"0:10:00\"", drive()).unwrap();
        assert_eq!(s.titles[0].output_name, "title_t07.mkv");
    }

    #[test]
    fn an_empty_disc_is_an_error_rather_than_an_empty_scan() {
        let e = parse_scan("MSG:5010,0,0,\"no disc\"", drive()).unwrap_err();
        assert!(e.0.contains("no titles"), "{}", e.0);
    }

    #[test]
    fn the_fingerprint_ignores_short_titles() {
        let s = parse_scan(INFO, drive()).unwrap();
        // the 2:30 menu loop is not part of what identifies the disc
        assert_eq!(s.duration_fingerprint(180_000), vec![1_275_000, 5_098_000]);
    }

    #[test]
    fn progress_is_a_fraction_of_the_maximum() {
        let p = parse_progress("PRGV:16384,32768,65536").unwrap();
        assert!((p.total - 0.5).abs() < 0.001);
        assert_eq!(parse_progress("PRGV:0,0,0"), None);
        assert_eq!(parse_progress("TCOUNT:3"), None);
    }

    #[test]
    fn operation_names_come_through_as_messages() {
        let p = parse_progress(r#"PRGT:5018,0,"Analyzing seamless segments""#).unwrap();
        assert_eq!(p.message.as_deref(), Some("Analyzing seamless segments"));
    }

    #[test]
    fn failures_are_pulled_out_of_the_message_stream() {
        let out = "MSG:1005,0,1,\"started\"\nMSG:5010,0,0,\"Failed to open disc\"\n";
        assert_eq!(parse_error(out).as_deref(), Some("Failed to open disc"));
        assert_eq!(parse_error("MSG:1005,0,1,\"started\""), None);
    }

    #[test]
    fn a_partial_rip_is_visible_even_though_the_exit_code_is_zero() {
        let out = r#"MSG:5036,0,3,"7 titles saved, 1 failed","%1 titles saved, %2 failed","7","1""#;
        assert_eq!(saved_titles(out), Some((7, 1)));
    }

    #[test]
    fn commands_carry_the_flags_that_make_output_parseable() {
        assert!(drives_command().has("-r"));
        let c = scan_command("disc:0", 120);
        assert!(c.has("--minlength=120"));
        assert!(c.has("info"));
        let c = rip_command("disc:0", Some(3), std::path::Path::new("/rip"), 120);
        assert_eq!(c.args.last().unwrap(), "/rip");
        assert!(c.has("3"), "{}", c.display());
        assert!(rip_command("disc:0", None, std::path::Path::new("/rip"), 120).has("all"));
    }
}

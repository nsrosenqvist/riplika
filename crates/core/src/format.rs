//! What each output format needs, gathered in one place.
//!
//! MP4 and Matroska differ in more ways than their extension: which muxer
//! writes them, what a text subtitle is called, whether the index belongs at
//! the front, which metadata the muxer can carry, and what has to be done to
//! the file afterwards. Those five facts were spread across the transcode
//! planner and the job pipeline as separate `if container ==` branches, so
//! adding a format meant finding all of them, and getting one wrong meant a
//! file that muxed happily and was wrong in a way only a player would show.
//!
//! Each format answers for itself here instead.

use crate::Result;
use crate::host::Fs;
use crate::model::{Container, Item, Media};
use std::path::Path;

pub trait Format: Send + Sync {
    /// The name the file takes.
    fn extension(&self) -> &'static str;

    /// What ffmpeg calls the muxer.
    ///
    /// Said outright rather than inferred from the file name, because a file
    /// is written to a `.part` path while it is being made and ffmpeg would
    /// have nothing to infer from.
    fn muxer(&self) -> &'static str;

    /// What a text subtitle track is called here.
    fn text_subtitle_codec(&self) -> &'static str;

    /// Options that belong to this format and no other.
    fn mux_options(&self) -> &'static [&'static str] {
        &[]
    }

    /// Can the muxer carry this piece of metadata itself?
    ///
    /// The tag names are the iTunes atoms', which is what MP4 wants. Matroska
    /// has its own vocabulary and a target for each level, and ffmpeg cannot
    /// write those, so it takes only the title - which becomes the segment's
    /// own Title element - and gets the rest afterwards.
    fn mux_carries(&self, _tag: &str) -> bool {
        true
    }

    /// Whatever the finished file still needs.
    ///
    /// Runs on the temporary file, before it is moved to its real name.
    fn finish(&self, _fs: &dyn Fs, _path: &Path, _media: &Media, _item: &Item) -> Result<()> {
        Ok(())
    }
}

pub struct Mp4;

impl Format for Mp4 {
    fn extension(&self) -> &'static str {
        "mp4"
    }
    fn muxer(&self) -> &'static str {
        "mp4"
    }
    fn text_subtitle_codec(&self) -> &'static str {
        "mov_text"
    }
    fn mux_options(&self) -> &'static [&'static str] {
        // HandBrake's "web optimized": the moov atom at the front, so a player
        // can start before the whole file has arrived.
        &["-movflags", "+faststart"]
    }
    fn finish(&self, fs: &dyn Fs, path: &Path, _media: &Media, _item: &Item) -> Result<()> {
        crate::transcode::mp4::drop_dangling_chapter_refs(fs, path).map(|_| ())
    }
}

pub struct Matroska;

impl Format for Matroska {
    fn extension(&self) -> &'static str {
        "mkv"
    }
    fn muxer(&self) -> &'static str {
        "matroska"
    }
    fn text_subtitle_codec(&self) -> &'static str {
        "srt"
    }
    fn mux_carries(&self, tag: &str) -> bool {
        tag == "title"
    }
    fn finish(&self, fs: &dyn Fs, path: &Path, media: &Media, item: &Item) -> Result<()> {
        crate::mkvtags::write(fs, path, media, item).map(|_| ())
    }
}

impl Container {
    pub fn format(self) -> &'static dyn Format {
        match self {
            Container::Mp4 => &Mp4,
            Container::Mkv => &Matroska,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_container_answers_for_itself() {
        for c in [Container::Mp4, Container::Mkv] {
            let f = c.format();
            assert!(!f.extension().is_empty());
            assert!(!f.muxer().is_empty());
            assert!(!f.text_subtitle_codec().is_empty());
        }
    }

    #[test]
    fn the_muxer_is_named_not_guessed_from_the_extension() {
        // matroska's muxer is not called "mkv", so inferring it from the name
        // would be wrong even before the ".part" suffix made it impossible
        assert_eq!(Container::Mkv.format().muxer(), "matroska");
        assert_eq!(Container::Mkv.format().extension(), "mkv");
        assert_eq!(Container::Mp4.format().muxer(), "mp4");
    }

    #[test]
    fn matroska_takes_only_the_title_from_the_muxer() {
        // the rest are the iTunes atoms' names, which mean nothing here, and
        // the ones that do mean something need targets ffmpeg cannot write
        let mkv = Container::Mkv.format();
        assert!(mkv.mux_carries("title"));
        for name in ["show", "season_number", "episode_sort", "media_type"] {
            assert!(!mkv.mux_carries(name), "matroska should not be given {name}");
        }
        let mp4 = Container::Mp4.format();
        for name in ["title", "show", "season_number", "episode_sort", "media_type"] {
            assert!(mp4.mux_carries(name), "mp4 wants {name}");
        }
    }

    #[test]
    fn only_mp4_asks_for_the_index_at_the_front() {
        assert!(Container::Mp4.format().mux_options().contains(&"+faststart"));
        assert!(Container::Mkv.format().mux_options().is_empty());
    }
}

//! The glyph table: the exact-match lookup that replaces statistical OCR.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use crate::subs::segment::Glyph;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub key: String,
    pub w: i32,
    pub h: i32,
    /// Base64 of one byte per pixel, 1 = ink.
    pub bits: String,
    /// The character(s) this glyph represents. `None` until labelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// How often the glyph was seen while building the table.
    #[serde(default)]
    pub count: u64,
    /// Label votes gathered during bootstrap, highest first when saved.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub votes: BTreeMap<String, u64>,
    /// Gap after this glyph above which a space follows.
    ///
    /// Learned per glyph because letters overhang differently: the tail of an
    /// `f` or `y` eats into the gap, so one global number splits `if you` into
    /// `ifyou` while leaving other pairs correct.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<i32>,
}

impl Entry {
    pub fn bitmap(&self) -> Vec<u8> {
        B64.decode(self.bits.as_bytes()).unwrap_or_default()
    }

    /// Fraction of votes that agree with the chosen label, 1.0 when unanimous.
    pub fn agreement(&self) -> f32 {
        let total: u64 = self.votes.values().sum();
        if total == 0 {
            return 0.0;
        }
        let top = self.votes.values().copied().max().unwrap_or(0);
        top as f32 / total as f32
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Table {
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub source: String,
    pub glyphs: Vec<Entry>,
    #[serde(skip)]
    index: BTreeMap<String, usize>,
}

fn one() -> u32 {
    1
}

impl Table {
    pub fn load(path: &Path) -> Result<Table, String> {
        let s = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut t: Table =
            serde_json::from_str(&s).map_err(|e| format!("{}: {e}", path.display()))?;
        t.reindex();
        Ok(t)
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut t = self.clone();
        // most-seen first keeps the review sheet in a useful order
        t.glyphs.sort_by(|a, b| b.count.cmp(&a.count).then(a.key.cmp(&b.key)));
        let s = serde_json::to_string_pretty(&t).map_err(|e| e.to_string())?;
        std::fs::write(path, s).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn reindex(&mut self) {
        self.index = self.glyphs.iter().enumerate().map(|(i, g)| (g.key.clone(), i)).collect();
    }

    pub fn get(&self, key: &str) -> Option<&Entry> {
        self.index.get(key).map(|&i| &self.glyphs[i])
    }

    pub fn observe(&mut self, g: &Glyph) -> usize {
        let key = g.key();
        if let Some(&i) = self.index.get(&key) {
            self.glyphs[i].count += 1;
            return i;
        }
        let e = Entry {
            key: key.clone(),
            w: g.w,
            h: g.h,
            bits: B64.encode(&g.bits),
            text: None,
            count: 1,
            votes: BTreeMap::new(),
            gap: None,
        };
        self.glyphs.push(e);
        let i = self.glyphs.len() - 1;
        self.index.insert(key, i);
        i
    }

    pub fn vote(&mut self, idx: usize, label: &str) {
        *self.glyphs[idx].votes.entry(label.to_string()).or_insert(0) += 1;
    }

    /// Adopt the majority vote as each glyph's label.
    ///
    /// When two labels each hold a real share of the votes the glyph is not
    /// mislabelled - the font simply draws both characters identically (capital
    /// I and lowercase l, most often). Record it as an ambiguity class
    /// `"l|I"`, most frequent first, and let context resolve it later.
    pub fn apply_votes(&mut self, min_agreement: f32) -> (usize, usize, usize) {
        let (mut set, mut ambiguous, mut skipped) = (0, 0, 0);
        for g in self.glyphs.iter_mut() {
            if g.votes.is_empty() {
                continue;
            }
            let total: u64 = g.votes.values().sum();
            let mut ranked: Vec<(&String, &u64)> = g.votes.iter().collect();
            ranked.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            let top = *ranked[0].1;

            if (top as f32 / total as f32) >= min_agreement {
                g.text = Some(ranked[0].0.clone());
                set += 1;
                continue;
            }
            // Two strong candidates that between them explain the glyph. Demand
            // real support for the runner-up: a handful of stray votes is noise
            // in the bootstrap source, not a genuine collision.
            if ranked.len() >= 2 {
                let second = *ranked[1].1;
                let covered = (top + second) as f32 / total as f32;
                if covered >= 0.97 && second >= 10 && (second as f32 / total as f32) >= 0.08 {
                    g.text = Some(format!("{}|{}", ranked[0].0, ranked[1].0));
                    ambiguous += 1;
                    continue;
                }
            }
            skipped += 1;
        }
        (set, ambiguous, skipped)
    }

    pub fn unlabelled(&self) -> usize {
        self.glyphs.iter().filter(|g| g.text.is_none()).count()
    }

    pub fn labelled(&self) -> usize {
        self.glyphs.iter().filter(|g| g.text.is_some()).count()
    }
}

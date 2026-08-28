//! A self-contained HTML page for reviewing and correcting glyph labels.
//!
//! This is the human gate. Everything else in the pipeline is deterministic;
//! this is where a person confirms the handful of shapes the machine is unsure
//! about, once per release font.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

use crate::subs::table::{Entry, Table};

fn png_data_uri(e: &Entry, zoom: usize) -> String {
    let bits = e.bitmap();
    let (w, h) = (e.w as usize, e.h as usize);
    let (ow, oh) = (w * zoom, h * zoom);
    let mut raw = vec![255u8; ow * oh];
    for y in 0..h {
        for x in 0..w {
            if bits.get(y * w + x).copied().unwrap_or(0) != 0 {
                for dy in 0..zoom {
                    for dx in 0..zoom {
                        raw[(y * zoom + dy) * ow + x * zoom + dx] = 0;
                    }
                }
            }
        }
    }
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, ow as u32, oh as u32);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::Eight);
        if let Ok(mut w) = enc.write_header() {
            let _ = w.write_image_data(&raw);
        }
    }
    format!("data:image/png;base64,{}", B64.encode(&buf))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn render(table: &Table, zoom: usize) -> String {
    let mut rows = String::new();
    let mut sorted: Vec<&Entry> = table.glyphs.iter().collect();
    // uncertain first - that is what a reviewer needs to look at
    sorted.sort_by(|a, b| {
        let ka = (a.text.is_some(), (a.agreement() * 1000.0) as i32);
        let kb = (b.text.is_some(), (b.agreement() * 1000.0) as i32);
        ka.cmp(&kb).then(b.count.cmp(&a.count))
    });

    for e in sorted {
        let label = e.text.clone().unwrap_or_default();
        let agree = if e.votes.is_empty() {
            "-".to_string()
        } else {
            format!("{:.0}%", e.agreement() * 100.0)
        };
        let cls = match (&e.text, e.agreement()) {
            (None, _) => "unknown",
            (Some(_), a) if a < 0.98 => "shaky",
            _ => "ok",
        };
        let alts: Vec<String> = e
            .votes
            .iter()
            .map(|(k, v)| format!("{}&times;{}", esc(k), v))
            .collect();
        rows.push_str(&format!(
            r#"<div class="g {cls}">
  <img src="{src}" alt="">
  <input value="{label}" data-key="{key}" size="4">
  <div class="meta">{count} seen &middot; {agree}</div>
  <div class="alts">{alts}</div>
</div>
"#,
            cls = cls,
            src = png_data_uri(e, zoom),
            label = esc(&label),
            key = esc(&e.key),
            count = e.count,
            agree = agree,
            alts = alts.join(" ")
        ));
    }

    format!(
        r#"<meta charset="utf-8">
<title>Glyph table review</title>
<style>
 :root {{ --bg:#fff; --fg:#111; --line:#d8d8d8; --warn:#b45309; --bad:#b91c1c; }}
 @media (prefers-color-scheme: dark) {{
   :root {{ --bg:#15171a; --fg:#e8e8e8; --line:#333; --warn:#f59e0b; --bad:#f87171; }}
 }}
 body {{ background:var(--bg); color:var(--fg); font:14px/1.4 system-ui,sans-serif; margin:24px; }}
 h1 {{ font-size:18px; margin:0 0 4px; }}
 .sub {{ opacity:.7; margin-bottom:18px; }}
 .grid {{ display:flex; flex-wrap:wrap; gap:10px; }}
 .g {{ border:1px solid var(--line); border-radius:8px; padding:8px; width:104px;
       text-align:center; background:var(--bg); }}
 .g img {{ background:#fff; max-width:88px; height:auto; image-rendering:pixelated;
           border-radius:3px; }}
 .g input {{ width:70px; text-align:center; font:15px monospace; margin-top:6px;
             background:var(--bg); color:var(--fg); border:1px solid var(--line);
             border-radius:4px; padding:2px; }}
 .meta {{ font-size:11px; opacity:.6; margin-top:4px; }}
 .alts {{ font-size:10px; opacity:.5; margin-top:2px; word-break:break-all; }}
 .unknown {{ border-color:var(--bad); }}
 .shaky   {{ border-color:var(--warn); }}
 #out {{ width:100%; height:150px; margin-top:18px; font:12px monospace;
         background:var(--bg); color:var(--fg); border:1px solid var(--line); }}
 button {{ font:14px system-ui; padding:6px 12px; margin-top:12px; }}
</style>
<h1>Glyph table review &mdash; {source}</h1>
<div class="sub">{total} glyphs &middot; {labelled} labelled &middot; {unlabelled} need a label.
Red = unlabelled, amber = votes disagreed. Fix any that are wrong, then press
<b>Copy corrections</b> and save them as a JSON file for <code>ripper label</code>.</div>
<div class="grid">
{rows}</div>
<button onclick="dump()">Copy corrections</button>
<textarea id="out" readonly></textarea>
<script>
function dump() {{
  const o = {{}};
  document.querySelectorAll('input[data-key]').forEach(i => {{
    if (i.value !== '') o[i.dataset.key] = i.value;
  }});
  const t = JSON.stringify(o, null, 2);
  document.getElementById('out').value = t;
  navigator.clipboard && navigator.clipboard.writeText(t);
}}
</script>
"#,
        source = esc(&table.source),
        total = table.glyphs.len(),
        labelled = table.labelled(),
        unlabelled = table.unlabelled(),
        rows = rows
    )
}

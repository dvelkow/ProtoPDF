//! Text of one page with glyph positions, used for search, selection and copy.

/// One glyph as drawn on the page. Coordinates are page points, origin top-left,
/// after page rotation (the same space the viewer draws in).
#[derive(Clone, Debug)]
pub struct TGlyph {
    pub s: String,
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl TGlyph {
    fn h(&self) -> f32 {
        (self.y1 - self.y0).abs()
    }
    fn cy(&self) -> f32 {
        (self.y0 + self.y1) * 0.5
    }
}

const NO_GLYPH: u32 = u32::MAX;

#[derive(Default)]
pub struct PageText {
    pub glyphs: Vec<TGlyph>,
    /// The page text, one entry per character, with spaces and line breaks inferred.
    raw: Vec<char>,
    /// Lower-cased copy of `raw`, same length.
    lower: Vec<char>,
    /// Glyph index that produced each character, `NO_GLYPH` for inferred separators.
    owner: Vec<u32>,
    /// First character index of each glyph.
    gstart: Vec<u32>,
}

fn lower1(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

impl PageText {
    pub fn build(glyphs: Vec<TGlyph>) -> Self {
        let mut raw = Vec::with_capacity(glyphs.len() + glyphs.len() / 5);
        let mut owner = Vec::with_capacity(raw.capacity());
        let mut gstart = Vec::with_capacity(glyphs.len());

        for (i, g) in glyphs.iter().enumerate() {
            if i > 0 {
                let p = &glyphs[i - 1];
                let h = p.h().max(g.h()).max(1.0);
                let same_line = (g.cy() - p.cy()).abs() < 0.6 * h;
                let sep = if !same_line {
                    Some('\n')
                } else if g.x0 - p.x1 > 0.18 * h || g.x0 < p.x0 - h {
                    Some(' ')
                } else {
                    None
                };
                if let Some(sep) = sep {
                    let prev_ws = raw.last().is_none_or(|c: &char| c.is_whitespace());
                    let cur_ws = g.s.chars().all(char::is_whitespace);
                    if sep == '\n' || (!prev_ws && !cur_ws) {
                        raw.push(sep);
                        owner.push(NO_GLYPH);
                    }
                }
            }
            gstart.push(raw.len() as u32);
            for c in g.s.chars() {
                raw.push(c);
                owner.push(i as u32);
            }
        }

        let lower = raw.iter().map(|c| lower1(*c)).collect();
        Self { glyphs, raw, lower, owner, gstart }
    }

    pub fn is_empty(&self) -> bool {
        self.glyphs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.raw.len()
    }

    /// All matches of `needle` (already lower-cased) as character ranges.
    pub fn find(&self, needle: &[char]) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        if needle.is_empty() || needle.len() > self.lower.len() {
            return out;
        }
        let hay = &self.lower;
        let mut i = 0;
        while i + needle.len() <= hay.len() {
            let hit = needle.iter().enumerate().all(|(k, n)| {
                let h = hay[i + k];
                // A line break in the page matches a space in the query.
                h == *n || (*n == ' ' && h == '\n')
            });
            if hit {
                out.push((i, i + needle.len()));
                i += needle.len();
            } else {
                i += 1;
            }
        }
        out
    }

    /// Highlight rectangles for the character range `[a, b)`, merged per line.
    pub fn rects(&self, a: usize, b: usize) -> Vec<[f32; 4]> {
        let mut out: Vec<[f32; 4]> = Vec::new();
        let mut last_glyph = NO_GLYPH;
        for i in a..b.min(self.owner.len()) {
            let gi = self.owner[i];
            if gi == NO_GLYPH || gi == last_glyph {
                continue;
            }
            last_glyph = gi;
            let g = &self.glyphs[gi as usize];
            let (x0, x1) = (g.x0.min(g.x1), g.x0.max(g.x1));
            let (y0, y1) = (g.y0.min(g.y1), g.y0.max(g.y1));
            if let Some(r) = out.last_mut() {
                let overlap = y0.max(r[1]) < y1.min(r[3]);
                let h = (r[3] - r[1]).max(1.0);
                if overlap && x0 >= r[0] - h && x0 <= r[2] + 2.0 * h {
                    r[0] = r[0].min(x0);
                    r[1] = r[1].min(y0);
                    r[2] = r[2].max(x1);
                    r[3] = r[3].max(y1);
                    continue;
                }
            }
            out.push([x0, y0, x1, y1]);
        }
        out
    }

    /// Character index of the glyph closest to the point.
    pub fn nearest_char(&self, x: f32, y: f32) -> Option<usize> {
        let mut best: Option<(f32, usize)> = None;
        for (i, g) in self.glyphs.iter().enumerate() {
            let (x0, x1) = (g.x0.min(g.x1), g.x0.max(g.x1));
            let (y0, y1) = (g.y0.min(g.y1), g.y0.max(g.y1));
            let dx = (x0 - x).max(0.0).max(x - x1);
            let dy = (y0 - y).max(0.0).max(y - y1);
            // Vertical distance counts more so a drag stays on its line.
            let d = dx * dx + 9.0 * dy * dy;
            if best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, i));
            }
        }
        best.map(|(_, i)| self.gstart[i] as usize)
    }

    /// End (exclusive) of the glyph that owns character `i`.
    pub fn glyph_end(&self, i: usize) -> usize {
        let Some(&gi) = self.owner.get(i) else { return self.raw.len() };
        let mut e = i + 1;
        while e < self.owner.len() && self.owner[e] == gi {
            e += 1;
        }
        e
    }

    /// The word around character `i` as a range `[a, b)`.
    pub fn word_range(&self, i: usize) -> (usize, usize) {
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let i = i.min(self.raw.len().saturating_sub(1));
        if self.raw.is_empty() || !is_word(self.raw[i]) {
            return (i, (i + 1).min(self.raw.len()));
        }
        let mut a = i;
        while a > 0 && is_word(self.raw[a - 1]) {
            a -= 1;
        }
        let mut b = i + 1;
        while b < self.raw.len() && is_word(self.raw[b]) {
            b += 1;
        }
        (a, b)
    }

    pub fn text_range(&self, a: usize, b: usize) -> String {
        let b = b.min(self.raw.len());
        if a >= b {
            return String::new();
        }
        let mut s = String::with_capacity(b - a);
        for c in &self.raw[a..b] {
            if *c == '\n' {
                s.push_str("\r\n");
            } else {
                s.push(*c);
            }
        }
        s
    }
}

pub fn lower_chars(s: &str) -> Vec<char> {
    s.chars().map(lower1).collect()
}

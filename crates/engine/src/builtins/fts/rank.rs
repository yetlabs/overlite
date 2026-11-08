//! ts_rank and ts_rank_cd, as Postgres computes them (src/backend/utils/adt/tsrank.c),
//! down to where it rounds to a float4 and where it carries a double, so that the
//! numbers come out the same.

use super::query::{Node, TsQuery, execute};
use super::vector::{MAX_POS, Pos, TsVector};
use crate::error::{Error, Result};
use overlite_core::SqlState;

/// The weights of D, C, B and A when none are given.
pub(crate) const DEFAULT_WEIGHTS: [f32; 4] = [0.1, 0.2, 0.4, 1.0];

/// The positions an entry without any stands for: calc_rank_and's.
const POS_NULL_AND: u16 = MAX_POS;

/// The weights a float4[] gives, D first: a negative one is the default.
///
/// # Errors
///
/// What Postgres answers for too short an array, a NULL in it, or a weight past 1.
pub(crate) fn weights(given: &[Option<f32>]) -> Result<[f32; 4]> {
    if given.len() < 4 {
        return Err(Error::with_state(
            SqlState::ARRAY_SUBSCRIPT_ERROR,
            "array of weight is too short",
        ));
    }
    let mut w = DEFAULT_WEIGHTS;
    for (i, g) in given.iter().take(4).enumerate() {
        let Some(g) = g else {
            return Err(Error::with_state(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                "array of weight must not contain nulls",
            ));
        };
        if *g >= 0.0 {
            w[i] = *g;
        }
        if w[i] > 1.0 {
            return Err(Error::with_state(
                SqlState::INVALID_PARAMETER_VALUE,
                "weight out of range",
            ));
        }
    }
    Ok(w)
}

/// The operands of a query by their text, sorted and each once: SortAndUniqItems.
fn unique_operands(q: &TsQuery) -> Vec<(&str, bool)> {
    let mut items: Vec<(&str, bool)> = q
        .operands()
        .into_iter()
        .filter_map(|n| match n {
            Node::Val { lexeme, prefix, .. } => Some((lexeme.as_str(), *prefix)),
            _ => None,
        })
        .collect();
    items.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    items.dedup_by(|a, b| a.0 == b.0);
    items
}

fn word_distance(w: i32) -> f32 {
    if w > 100 {
        return 1e-30;
    }
    (1.0 / (1.005 + 0.05 * (f64::from(w as f32) / 1.5 - 2.0).exp())) as f32
}

fn calc_rank_and(w: &[f32; 4], t: &TsVector, q: &TsQuery) -> f32 {
    let items = unique_operands(q);
    if items.len() < 2 {
        return calc_rank_or(w, t, q);
    }
    let null = [Pos::new(POS_NULL_AND, 0)];
    let mut pos: Vec<Option<(&[Pos], bool)>> = vec![None; items.len()];
    let mut res: f32 = -1.0;
    for i in 0..items.len() {
        for entry in t.find(items[i].0, items[i].1) {
            let here: (&[Pos], bool) = if entry.positions.is_empty() {
                (&null, true)
            } else {
                (&entry.positions, false)
            };
            pos[i] = Some(here);
            for &(ct, k_null) in pos[..i].iter().flatten() {
                for l in here.0 {
                    for p in ct {
                        let mut dist = (i32::from(l.pos()) - i32::from(p.pos())).abs();
                        if dist != 0 || here.1 || k_null {
                            if dist == 0 {
                                dist = i32::from(MAX_POS) + 1;
                            }
                            let prod = w[usize::from(l.weight())] * w[usize::from(p.weight())] * word_distance(dist);
                            let curw = f64::from(prod).sqrt() as f32;
                            res = if res < 0.0 {
                                curw
                            } else {
                                (1.0 - (1.0 - f64::from(res)) * (1.0 - f64::from(curw))) as f32
                            };
                        }
                    }
                }
            }
        }
    }
    res
}

fn calc_rank_or(w: &[f32; 4], t: &TsVector, q: &TsQuery) -> f32 {
    let items = unique_operands(q);
    let null = [Pos::new(0, 0)];
    let mut res: f32 = 0.0;
    for (lexeme, prefix) in &items {
        for entry in t.find(lexeme, *prefix) {
            let post: &[Pos] = if entry.positions.is_empty() {
                &null
            } else {
                &entry.positions
            };
            let mut resj: f32 = 0.0;
            let mut wjm: f32 = -1.0;
            let mut jm: i32 = 0;
            for (j, p) in post.iter().enumerate() {
                let wp = w[usize::from(p.weight())];
                let jj = (j as i32 + 1) * (j as i32 + 1);
                resj += wp / jj as f32;
                if wp > wjm {
                    wjm = wp;
                    jm = j as i32;
                }
            }
            let jj = (jm + 1) * (jm + 1);
            let part = wjm + resj - wjm / jj as f32;
            res = (f64::from(res) + f64::from(part) / 1.644_934_066_85) as f32;
        }
    }
    if !items.is_empty() {
        res /= items.len() as f32;
    }
    res
}

/// ts_rank.
pub(crate) fn ts_rank(w: &[f32; 4], t: &TsVector, q: &TsQuery, method: i32) -> f32 {
    let Some(root) = &q.root else {
        return 0.0;
    };
    if t.entries.is_empty() {
        return 0.0;
    }
    let mut res = if matches!(root, Node::And(..) | Node::Phrase(..)) {
        calc_rank_and(w, t, q)
    } else {
        calc_rank_or(w, t, q)
    };
    if res < 0.0 {
        res = 1e-20;
    }
    let size = t.entries.len();
    if method & 1 != 0 {
        res = (f64::from(res) / ((t.count_length() as f64 + 1.0).ln() / 2f64.ln())) as f32;
    }
    if method & 2 != 0 {
        let len = t.count_length();
        if len > 0 {
            res /= len as f32;
        }
    }
    if method & 8 != 0 {
        res /= size as f32;
    }
    if method & 16 != 0 {
        res = (f64::from(res) / ((size as f64 + 1.0).ln() / 2f64.ln())) as f32;
    }
    if method & 32 != 0 {
        res /= res + 1.0;
    }
    res
}

/// A position of the document with the operands found there.
struct DocPos {
    pos: Pos,
    entry: usize,
    items: Vec<usize>,
}

/// get_docrep: every position of a lexeme some operand finds, by its weights, in order.
fn doc_representation(t: &TsVector, operands: &[&Node]) -> Vec<DocPos> {
    let mut doc: Vec<DocPos> = Vec::new();
    for (i, n) in operands.iter().enumerate() {
        let Node::Val { lexeme, weight, prefix } = n else {
            continue;
        };
        let range = t.find_range(lexeme, *prefix);
        for entry in range {
            let e = &t.entries[entry];
            let null = [Pos::new(0, 0)];
            let post: &[Pos] = if e.positions.is_empty() { &null } else { &e.positions };
            for p in post {
                if *weight == 0 || weight & (1 << p.weight()) != 0 {
                    doc.push(DocPos {
                        pos: *p,
                        entry,
                        items: vec![i],
                    });
                }
            }
        }
    }
    doc.sort_by_key(|d| (d.pos.pos(), d.pos.weight(), d.entry));
    let mut out: Vec<DocPos> = Vec::with_capacity(doc.len());
    for d in doc {
        match out.last_mut() {
            Some(last) if last.pos == d.pos && last.entry == d.entry => last.items.extend(d.items),
            _ => out.push(d),
        }
    }
    out
}

/// What the cover search knows of each operand.
#[derive(Clone, Default)]
struct OperandData {
    exists: bool,
    pos: Vec<u16>,
}

fn fill(data: &mut [OperandData], d: &DocPos) {
    for &i in &d.items {
        let o = &mut data[i];
        o.exists = true;
        if !o.pos.contains(&d.pos.pos()) {
            o.pos.push(d.pos.pos());
        }
    }
}

fn holds(root: &Node, data: &[OperandData]) -> bool {
    execute(root, &mut |i, _| {
        let o = &data[i];
        if !o.exists {
            return None;
        }
        let mut p = o.pos.clone();
        p.sort_unstable();
        Some(Some(p))
    })
}

/// The next cover from ext_pos: its first and last index into doc, and positions.
fn cover(doc: &[DocPos], root: &Node, n: usize, ext_pos: &mut usize) -> Option<(usize, usize, i32, i32)> {
    loop {
        let mut data = vec![OperandData::default(); n];
        let mut q = 0i32;
        let mut end = None;
        let mut lastpos = *ext_pos;
        for (k, d) in doc.iter().enumerate().skip(*ext_pos) {
            fill(&mut data, d);
            if holds(root, &data) {
                if i32::from(d.pos.pos()) > q {
                    q = i32::from(d.pos.pos());
                    end = Some(k);
                    lastpos = k;
                }
                break;
            }
        }
        let end = end?;
        let mut data = vec![OperandData::default(); n];
        let mut p = i32::MAX;
        let mut begin = lastpos;
        let mut k = lastpos as isize;
        while k >= *ext_pos as isize {
            let d = &doc[k as usize];
            fill(&mut data, d);
            if holds(root, &data) {
                if i32::from(d.pos.pos()) < p {
                    begin = k as usize;
                    p = i32::from(d.pos.pos());
                }
                break;
            }
            k -= 1;
        }
        if p <= q {
            *ext_pos = (k + 1) as usize;
            return Some((begin, end, p, q));
        }
        *ext_pos += 1;
    }
}

/// ts_rank_cd.
pub(crate) fn ts_rank_cd(w: &[f32; 4], t: &TsVector, q: &TsQuery, method: i32) -> f32 {
    let Some(root) = &q.root else {
        return 0.0;
    };
    let inv: Vec<f64> = w.iter().map(|x| 1.0 / f64::from(*x)).collect();
    let operands = q.operands();
    let doc = doc_representation(t, &operands);
    if doc.is_empty() {
        return 0.0;
    }
    let mut wdoc = 0.0f64;
    let mut sum_dist = 0.0f64;
    let mut prev_ext = 0.0f64;
    let mut n_extent = 0;
    let mut ext_pos = 0usize;
    while let Some((begin, end, p, q)) = cover(&doc, root, operands.len(), &mut ext_pos) {
        let inv_sum: f64 = doc[begin..=end].iter().map(|d| inv[usize::from(d.pos.weight())]).sum();
        let cpos = (end - begin + 1) as f64 / inv_sum;
        let mut noise = (q - p) - (end - begin) as i32;
        if noise < 0 {
            noise = (end - begin) as i32 / 2;
        }
        wdoc += cpos / f64::from(1 + noise);
        let cur = f64::from(q + p) / 2.0;
        if n_extent > 0 && cur > prev_ext {
            sum_dist += 1.0 / (cur - prev_ext);
        }
        prev_ext = cur;
        n_extent += 1;
    }
    let size = t.entries.len();
    if method & 1 != 0 && size > 0 {
        wdoc /= (t.count_length() as f64 + 1.0).ln();
    }
    if method & 2 != 0 {
        let len = t.count_length();
        if len > 0 {
            wdoc /= len as f64;
        }
    }
    if method & 4 != 0 && n_extent > 0 && sum_dist > 0.0 {
        wdoc /= f64::from(n_extent) / sum_dist;
    }
    if method & 8 != 0 && size > 0 {
        wdoc /= size as f64;
    }
    if method & 16 != 0 && size > 0 {
        wdoc /= (size as f64 + 1.0).ln() / 2f64.ln();
    }
    if method & 32 != 0 {
        wdoc /= wdoc + 1.0;
    }
    wdoc as f32
}

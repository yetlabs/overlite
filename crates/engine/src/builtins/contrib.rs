//! What the contrib extensions this server takes provide: unaccent's function and
//! pg_trgm's, as their C sources compute them.

use crate::catalog::scalar_fn;
use crate::error::Result;
use overlite_core::Value;
use rusqlite::Connection;
use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;

/// unaccent's rules for the Latin letters: each a letter, what it becomes, and a tab.
const UNACCENT_RULES: &str = "ÀA	ÁA	ÂA	ÃA	ÄA	ÅA	ÆAE	ÇC	ÈE	ÉE	ÊE	ËE	ÌI	ÍI	ÎI	ÏI	ÐD	ÑN	ÒO	ÓO	ÔO	ÕO	ÖO	ØO	ÙU	ÚU	ÛU	ÜU	ÝY	ÞTH	ßss	àa	áa	âa	ãa	äa	åa	æae	çc	èe	ée	êe	ëe	ìi	íi	îi	ïi	ðd	ñn	òo	óo	ôo	õo	öo	øo	ùu	úu	ûu	üu	ýy	þth	ÿy	ĀA	āa	ĂA	ăa	ĄA	ąa	ĆC	ćc	ĈC	ĉc	ĊC	ċc	ČC	čc	ĎD	ďd	ĐD	đd	ĒE	ēe	ĔE	ĕe	ĖE	ėe	ĘE	ęe	ĚE	ěe	ĜG	ĝg	ĞG	ğg	ĠG	ġg	ĢG	ģg	ĤH	ĥh	ĦH	ħh	ĨI	ĩi	ĪI	īi	ĬI	ĭi	ĮI	įi	İI	ıi	ĲIJ	ĳij	ĴJ	ĵj	ĶK	ķk	ĸq	ĹL	ĺl	ĻL	ļl	ĽL	ľl	ĿL	ŀl	ŁL	łl	ŃN	ńn	ŅN	ņn	ŇN	ňn	ŉ'n	ŊN	ŋn	ŌO	ōo	ŎO	ŏo	ŐO	őo	ŒOE	œoe	ŔR	ŕr	ŖR	ŗr	ŘR	řr	ŚS	śs	ŜS	ŝs	ŞS	şs	ŠS	šs	ŢT	ţt	ŤT	ťt	ŦT	ŧt	ŨU	ũu	ŪU	ūu	ŬU	ŭu	ŮU	ůu	ŰU	űu	ŲU	ųu	ŴW	ŵw	ŶY	ŷy	ŸY	ŹZ	źz	ŻZ	żz	ŽZ	žz	ſs	ƠO	ơo	ƯU	ưu	ǄDZ	ǅDz	ǆdz	ǇLJ	ǈLj	ǉlj	ǊNJ	ǋNj	ǌnj	ǍA	ǎa	ǏI	ǐi	ǑO	ǒo	ǓU	ǔu	ǕU	ǖu	ǗU	ǘu	ǙU	ǚu	ǛU	ǜu	ǞA	ǟa	ǠA	ǡa	ǦG	ǧg	ǨK	ǩk	ǪO	ǫo	ǬO	ǭo	ǰj	ǱDZ	ǲDz	ǳdz	ǴG	ǵg	ǸN	ǹn	ǺA	ǻa	ȀA	ȁa	ȂA	ȃa	ȄE	ȅe	ȆE	ȇe	ȈI	ȉi	ȊI	ȋi	ȌO	ȍo	ȎO	ȏo	ȐR	ȑr	ȒR	ȓr	ȔU	ȕu	ȖU	ȗu	ȘS	șs	ȚT	țt	ȞH	ȟh	ȦA	ȧa	ȨE	ȩe	ȪO	ȫo	ȬO	ȭo	ȮO	ȯo	ȰO	ȱo	ȲY	ȳy	ḀA	ḁa	ḂB	ḃb	ḄB	ḅb	ḆB	ḇb	ḈC	ḉc	ḊD	ḋd	ḌD	ḍd	ḎD	ḏd	ḐD	ḑd	ḒD	ḓd	ḔE	ḕe	ḖE	ḗe	ḘE	ḙe	ḚE	ḛe	ḜE	ḝe	ḞF	ḟf	ḠG	ḡg	ḢH	ḣh	ḤH	ḥh	ḦH	ḧh	ḨH	ḩh	ḪH	ḫh	ḬI	ḭi	ḮI	ḯi	ḰK	ḱk	ḲK	ḳk	ḴK	ḵk	ḶL	ḷl	ḸL	ḹl	ḺL	ḻl	ḼL	ḽl	ḾM	ḿm	ṀM	ṁm	ṂM	ṃm	ṄN	ṅn	ṆN	ṇn	ṈN	ṉn	ṊN	ṋn	ṌO	ṍo	ṎO	ṏo	ṐO	ṑo	ṒO	ṓo	ṔP	ṕp	ṖP	ṗp	ṘR	ṙr	ṚR	ṛr	ṜR	ṝr	ṞR	ṟr	ṠS	ṡs	ṢS	ṣs	ṤS	ṥs	ṦS	ṧs	ṨS	ṩs	ṪT	ṫt	ṬT	ṭt	ṮT	ṯt	ṰT	ṱt	ṲU	ṳu	ṴU	ṵu	ṶU	ṷu	ṸU	ṹu	ṺU	ṻu	ṼV	ṽv	ṾV	ṿv	ẀW	ẁw	ẂW	ẃw	ẄW	ẅw	ẆW	ẇw	ẈW	ẉw	ẊX	ẋx	ẌX	ẍx	ẎY	ẏy	ẐZ	ẑz	ẒZ	ẓz	ẔZ	ẕz	ẖh	ẗt	ẘw	ẙy	ẛs	ẠA	ạa	ẢA	ảa	ẤA	ấa	ẦA	ầa	ẨA	ẩa	ẪA	ẫa	ẬA	ậa	ẮA	ắa	ẰA	ằa	ẲA	ẳa	ẴA	ẵa	ẶA	ặa	ẸE	ẹe	ẺE	ẻe	ẼE	ẽe	ẾE	ếe	ỀE	ềe	ỂE	ểe	ỄE	ễe	ỆE	ệe	ỈI	ỉi	ỊI	ịi	ỌO	ọo	ỎO	ỏo	ỐO	ốo	ỒO	ồo	ỔO	ổo	ỖO	ỗo	ỘO	ộo	ỚO	ớo	ỜO	ờo	ỞO	ởo	ỠO	ỡo	ỢO	ợo	ỤU	ụu	ỦU	ủu	ỨU	ứu	ỪU	ừu	ỬU	ửu	ỮU	ữu	ỰU	ựu	ỲY	ỳy	ỴY	ỵy	ỶY	ỷy	ỸY	ỹy	";

fn unaccent_map() -> &'static HashMap<char, &'static str> {
    static MAP: OnceLock<HashMap<char, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| {
        UNACCENT_RULES
            .split('\t')
            .filter_map(|rule| {
                let mut chars = rule.char_indices();
                let (_, c) = chars.next()?;
                let at = chars.next()?.0;
                Some((c, &rule[at..]))
            })
            .collect()
    })
}

/// unaccent(text): each accented Latin letter as its letter alone.
pub(crate) fn unaccent(s: &str) -> String {
    let map = unaccent_map();
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match map.get(&c) {
            Some(r) => out.push_str(r),
            None => out.push(c),
        }
    }
    out
}

/// The trigrams of each word, in order: a word is a run of letters and digits, in
/// lower case, with two spaces before it and one after, as pg_trgm pads it.
fn word_trigrams(s: &str) -> Vec<Vec<String>> {
    let lower = s.to_lowercase();
    let mut words = Vec::new();
    for word in lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
        let padded: Vec<char> = "  ".chars().chain(word.chars()).chain(" ".chars()).collect();
        words.push(padded.windows(3).map(|w| w.iter().collect()).collect());
    }
    words
}

fn trigram_set(s: &str) -> BTreeSet<String> {
    word_trigrams(s).into_iter().flatten().collect()
}

/// count / (len1 + len2 - count), pg_trgm's CALCSML.
fn calc(count: usize, len1: usize, len2: usize) -> f64 {
    let union = len1 + len2 - count;
    if union == 0 { 0.0 } else { count as f64 / union as f64 }
}

/// similarity(a, b).
pub(crate) fn similarity(a: &str, b: &str) -> f64 {
    let (x, y) = (trigram_set(a), trigram_set(b));
    calc(x.intersection(&y).count(), x.len(), y.len())
}

/// word_similarity(a, b), or strict_word_similarity when strict: the greatest
/// similarity between a's trigrams and a run of b's, which for strict starts and ends
/// at b's word boundaries.
pub(crate) fn word_similarity(a: &str, b: &str, strict: bool) -> f64 {
    let x = trigram_set(a);
    if x.is_empty() {
        return 0.0;
    }
    let words = word_trigrams(b);
    let mut best = 0.0f64;
    if strict {
        for i in 0..words.len() {
            let mut ext = BTreeSet::new();
            for w in &words[i..] {
                ext.extend(w.iter().cloned());
                best = best.max(calc(x.intersection(&ext).count(), x.len(), ext.len()));
            }
        }
        return best;
    }
    let flat: Vec<&String> = words.iter().flatten().collect();
    for i in 0..flat.len() {
        let mut ext = BTreeSet::new();
        for t in &flat[i..] {
            ext.insert(*t);
            let common = ext.iter().filter(|t| x.contains(**t)).count();
            best = best.max(calc(common, x.len(), ext.len()));
        }
    }
    best
}

/// show_trgm(text): the trigrams as a text array, sorted.
fn show_trgm(s: &str) -> String {
    let items: Vec<String> = trigram_set(s)
        .into_iter()
        .map(|t| {
            if t.chars()
                .any(|c| c.is_whitespace() || matches!(c, '"' | '\\' | ',' | '{' | '}'))
            {
                ["\"", &t.replace('\\', "\\\\").replace('"', "\\\""), "\""].concat()
            } else {
                t
            }
        })
        .collect();
    ["{", &items.join(","), "}"].concat()
}

/// Registers the functions and the operators' functions.
pub(crate) fn register(conn: &Connection) -> Result<()> {
    let text = |v: &Value| v.to_string();
    scalar_fn(conn, "unaccent", 1, move |a| {
        Ok(if a[0].is_null() {
            Value::Null
        } else {
            Value::Text(unaccent(&text(&a[0])))
        })
    })?;
    scalar_fn(conn, "unaccent", 2, move |a| {
        Ok(if a[1].is_null() {
            Value::Null
        } else {
            Value::Text(unaccent(&text(&a[1])))
        })
    })?;
    let two = |a: &[Value]| (!a[0].is_null() && !a[1].is_null()).then(|| (a[0].to_string(), a[1].to_string()));
    scalar_fn(conn, "similarity", 2, move |a| {
        Ok(two(a).map_or(Value::Null, |(x, y)| Value::Float(similarity(&x, &y))))
    })?;
    scalar_fn(conn, "word_similarity", 2, move |a| {
        Ok(two(a).map_or(Value::Null, |(x, y)| Value::Float(word_similarity(&x, &y, false))))
    })?;
    scalar_fn(conn, "strict_word_similarity", 2, move |a| {
        Ok(two(a).map_or(Value::Null, |(x, y)| Value::Float(word_similarity(&x, &y, true))))
    })?;
    scalar_fn(conn, "show_trgm", 1, move |a| {
        Ok(if a[0].is_null() {
            Value::Null
        } else {
            Value::Text(show_trgm(&text(&a[0])))
        })
    })?;
    scalar_fn(conn, "show_limit", 0, |_| Ok(Value::Float(0.3)))?;
    // The operators, by what the rewrite calls them: op is %, <%, %>, <<%, %>>,
    // <->, <<->, <->>, <<<-> or <->>>.
    scalar_fn(conn, "overlite_trgm_match", 3, trgm)?;
    scalar_fn(conn, "overlite_trgm_dist", 3, trgm)?;
    Ok(())
}

/// What an operator of pg_trgm answers, by the operator.
fn trgm(a: &[Value]) -> Result<Value> {
    {
        let two = |a: &[Value]| (!a[0].is_null() && !a[1].is_null()).then(|| (a[0].to_string(), a[1].to_string()));
        let Some((x, y)) = two(a) else { return Ok(Value::Null) };
        Ok(match a[2].to_string().as_str() {
            "%" => Value::Bool(similarity(&x, &y) >= 0.3),
            "<%" => Value::Bool(word_similarity(&x, &y, false) >= 0.6),
            "%>" => Value::Bool(word_similarity(&y, &x, false) >= 0.6),
            "<<%" => Value::Bool(word_similarity(&x, &y, true) >= 0.5),
            "%>>" => Value::Bool(word_similarity(&y, &x, true) >= 0.5),
            "<->" => Value::Float(1.0 - similarity(&x, &y)),
            "<<->" => Value::Float(1.0 - word_similarity(&x, &y, false)),
            "<->>" => Value::Float(1.0 - word_similarity(&y, &x, false)),
            "<<<->" => Value::Float(1.0 - word_similarity(&x, &y, true)),
            "<->>>" => Value::Float(1.0 - word_similarity(&y, &x, true)),
            _ => Value::Null,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unaccent_letters() {
        assert_eq!(unaccent("Hôtel Ærø ß"), "Hotel AEro ss");
    }

    #[test]
    fn trigrams() {
        // The values of pg_trgm's documentation.
        assert!((word_similarity("word", "two words", false) - 0.8).abs() < 1e-9);
        assert!((word_similarity("word", "two words", true) - 4.0 / 7.0).abs() < 1e-9);
        assert!((similarity("word", "two words") - 4.0 / 11.0).abs() < 1e-9);
        assert_eq!(show_trgm("cat"), r#"{"  c"," ca","at ",cat}"#);
    }
}

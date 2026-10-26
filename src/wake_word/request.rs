//! A training request: the body of `POST /wake-words`, read and checked into a [`TrainingSpec`]
//! (`docs/engines/wake-word.md`, "Request").  Every refusal names the field it is about.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::recipe::{default_settings, Language};
use super::settings::{self, Settings};

/// The manifest's author when the request names none.
pub const DEFAULT_AUTHOR: &str = "studio-worker";
/// The manifest's website when the request names none.
pub const DEFAULT_WEBSITE: &str = "https://github.com/webbertakken/studio-worker";
/// Webber, 2026-10-25.  Safe range: above 0, at most 10.
pub const DEFAULT_TARGET_FALSE_ACCEPTS_PER_HOUR: f64 = 0.5;
const MAX_TARGET_FALSE_ACCEPTS_PER_HOUR: f64 = 10.0;
const PHRASE_CHARS: std::ops::RangeInclusive<usize> = 2..=64;
const MAX_NAME_CHARS: usize = 48;
const MAX_SPELLINGS: usize = 8;
const MAX_NEGATIVES: usize = 32;
const MAX_AUTHOR_CHARS: usize = 128;
const MAX_WEBSITE_CHARS: usize = 256;
const MAX_VOICE_SAMPLE_SETS: usize = 8;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Body {
    phrase: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    languages: Option<Vec<String>>,
    #[serde(default)]
    spellings: Option<BTreeMap<String, Vec<String>>>,
    #[serde(default)]
    negatives: Option<Vec<String>>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    website: Option<String>,
    #[serde(default)]
    target_false_accepts_per_hour: Option<f64>,
    #[serde(default)]
    settings: Option<serde_json::Value>,
    #[serde(default)]
    voice_samples: Vec<String>,
}

/// A checked training request.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrainingSpec {
    pub phrase: String,
    /// The files' name: `<name>.tflite`, `<name>.json`.
    pub name: String,
    /// The manifest's `wake_word`.
    pub display_name: String,
    pub languages: Vec<Language>,
    /// What each language's voices read, in the request's language order.
    pub spellings: BTreeMap<Language, Vec<String>>,
    pub negatives: Vec<String>,
    pub author: String,
    pub website: String,
    pub target_false_accepts_per_hour: f64,
    pub settings: Settings,
    /// The operator's recording sets this training learns from.
    pub voice_samples: Vec<String>,
}

/// Why a request is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid(pub String);

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn invalid<T>(message: impl Into<String>) -> Result<T, Invalid> {
    Err(Invalid(message.into()))
}

/// Collapse runs of whitespace and trim.
fn tidy(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A phrase, spelling or near miss: 2 to 64 letters, spaces, apostrophes and hyphens.
fn spoken(field: &str, text: &str) -> Result<String, Invalid> {
    let text = tidy(text);
    let chars = text.chars().count();
    if !PHRASE_CHARS.contains(&chars) {
        return invalid(format!(
            "{field} must be {} to {} characters, not {chars}",
            PHRASE_CHARS.start(),
            PHRASE_CHARS.end()
        ));
    }
    if let Some(bad) = text
        .chars()
        .find(|c| !(c.is_alphabetic() || matches!(c, ' ' | '\'' | '-' | '\u{2019}')))
    {
        return invalid(format!(
            "{field} may hold letters, spaces, apostrophes and hyphens only, not {bad:?}"
        ));
    }
    Ok(text)
}

/// The phrase as a file name: lower case, every run of other characters one `_`.
pub fn slug(phrase: &str) -> String {
    let mut slug = String::new();
    for c in phrase.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('_') {
            slug.push('_');
        }
    }
    let slug: String = slug
        .trim_matches('_')
        .chars()
        .take(MAX_NAME_CHARS)
        .collect();
    slug.trim_end_matches('_').to_string()
}

/// `hey runa` → `Hey Runa`.
fn title_case(phrase: &str) -> String {
    phrase
        .split(' ')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn free_text(field: &str, text: &str, max: usize) -> Result<String, Invalid> {
    let text = text.trim().to_string();
    if text.is_empty() || text.chars().count() > max {
        return invalid(format!("{field} must be 1 to {max} characters"));
    }
    if text.chars().any(char::is_control) {
        return invalid(format!("{field} may not hold control characters"));
    }
    Ok(text)
}

/// Read and check a request body.
pub fn parse(body: &[u8]) -> Result<TrainingSpec, Invalid> {
    let body: Body = serde_json::from_slice(body).map_err(|e| Invalid(format!("{e}")))?;
    let phrase = spoken("phrase", &body.phrase)?.to_lowercase();

    let name = match body.name {
        Some(name) => {
            let ok = !name.is_empty()
                && name.len() <= MAX_NAME_CHARS
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            if !ok {
                return invalid(format!(
                    "name must be 1 to {MAX_NAME_CHARS} of a-z, 0-9 and _, not {name:?}"
                ));
            }
            name
        }
        None => {
            let name = slug(&phrase);
            if name.is_empty() {
                return invalid(
                    "name: the phrase has no letters a-z to name the files by; give a name",
                );
            }
            name
        }
    };
    let display_name = match body.display_name {
        Some(text) => free_text("displayName", &text, *PHRASE_CHARS.end())?,
        None => title_case(&phrase),
    };

    let codes = body.languages.unwrap_or_else(|| vec!["en".into()]);
    if codes.is_empty() {
        return invalid("languages must name at least one language");
    }
    let mut languages = Vec::new();
    for code in &codes {
        let Some(language) = Language::parse(code) else {
            let known: Vec<_> = Language::ALL.iter().map(|l| l.code()).collect();
            return invalid(format!(
                "languages: {code:?} is not one of {}",
                known.join(", ")
            ));
        };
        if languages.contains(&language) {
            return invalid(format!("languages: {code} is named twice"));
        }
        languages.push(language);
    }

    let mut given = body.spellings.unwrap_or_default();
    let mut spellings = BTreeMap::new();
    for language in &languages {
        let list = match given.remove(language.code()) {
            Some(list) => {
                let field = format!("spellings.{}", language.code());
                if list.is_empty() || list.len() > MAX_SPELLINGS {
                    return invalid(format!("{field} must hold 1 to {MAX_SPELLINGS} texts"));
                }
                list.iter()
                    .map(|text| spoken(&field, text))
                    .collect::<Result<Vec<_>, _>>()?
            }
            None => vec![phrase.clone()],
        };
        spellings.insert(*language, list);
    }
    if let Some(code) = given.keys().next() {
        return invalid(format!(
            "spellings.{code}: {code} is not one of the languages asked for"
        ));
    }

    let negatives = body.negatives.unwrap_or_default();
    if negatives.len() > MAX_NEGATIVES {
        return invalid(format!(
            "negatives may hold at most {MAX_NEGATIVES} phrases, not {}",
            negatives.len()
        ));
    }
    let mut checked: Vec<String> = Vec::new();
    for text in &negatives {
        let text = spoken("negatives", text)?.to_lowercase();
        let positive = text == phrase
            || spellings
                .values()
                .flatten()
                .any(|s| s.to_lowercase() == text);
        if positive {
            return invalid(format!(
                "negatives: {text:?} is the phrase itself, or one of its spellings"
            ));
        }
        if checked.contains(&text) {
            return invalid(format!("negatives: {text:?} is named twice"));
        }
        checked.push(text);
    }
    let mut slugs: BTreeMap<String, &String> = BTreeMap::new();
    for text in &checked {
        let name = slug(text);
        if name.is_empty() {
            return invalid(format!(
                "negatives: {text:?} needs a letter a-z to name its samples by"
            ));
        }
        if let Some(other) = slugs.insert(name.clone(), text) {
            return invalid(format!(
                "negatives: {other:?} and {text:?} would share the name {name}"
            ));
        }
    }

    let author = match body.author {
        Some(text) => free_text("author", &text, MAX_AUTHOR_CHARS)?,
        None => DEFAULT_AUTHOR.into(),
    };
    let website = match body.website {
        Some(text) => {
            let text = free_text("website", &text, MAX_WEBSITE_CHARS)?;
            let scheme_ok = url::Url::parse(&text)
                .map(|u| matches!(u.scheme(), "http" | "https"))
                .unwrap_or(false);
            if !scheme_ok {
                return invalid("website must be an http(s) URL");
            }
            text
        }
        None => DEFAULT_WEBSITE.into(),
    };
    let target = body
        .target_false_accepts_per_hour
        .unwrap_or(DEFAULT_TARGET_FALSE_ACCEPTS_PER_HOUR);
    if !(target.is_finite() && target > 0.0 && target <= MAX_TARGET_FALSE_ACCEPTS_PER_HOUR) {
        return invalid(format!(
            "targetFalseAcceptsPerHour must be above 0 and at most {MAX_TARGET_FALSE_ACCEPTS_PER_HOUR}"
        ));
    }

    if body.voice_samples.len() > MAX_VOICE_SAMPLE_SETS {
        return invalid(format!(
            "voiceSamples may name at most {MAX_VOICE_SAMPLE_SETS} sets, not {}",
            body.voice_samples.len()
        ));
    }
    let mut voice_samples: Vec<String> = Vec::new();
    for set in body.voice_samples {
        crate::voice_samples::take::check_set(&set)
            .map_err(|bad| Invalid(format!("voiceSamples: {}", bad.0)))?;
        if voice_samples.contains(&set) {
            return invalid(format!("voiceSamples: {set} is named twice"));
        }
        voice_samples.push(set);
    }

    let settings =
        settings::with_overrides(&default_settings(), body.settings.as_ref()).map_err(Invalid)?;
    for language in &languages {
        if !settings.prosody.contains_key(language.code()) {
            return invalid(format!(
                "settings.prosody.{} is needed for a language asked for",
                language.code()
            ));
        }
    }

    Ok(TrainingSpec {
        phrase,
        name,
        display_name,
        languages,
        spellings,
        negatives: checked,
        author,
        website,
        target_false_accepts_per_hour: target,
        voice_samples,
        settings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parsed(body: serde_json::Value) -> Result<TrainingSpec, Invalid> {
        parse(body.to_string().as_bytes())
    }

    fn refused(body: serde_json::Value) -> String {
        parsed(body).unwrap_err().0
    }

    #[test]
    fn a_bare_phrase_takes_every_default() {
        let spec = parsed(json!({"phrase": "  Hey   Runa "})).unwrap();
        assert_eq!(spec.phrase, "hey runa");
        assert_eq!(spec.name, "hey_runa");
        assert_eq!(spec.display_name, "Hey Runa");
        assert_eq!(spec.languages, [Language::En]);
        assert_eq!(spec.spellings[&Language::En], ["hey runa"]);
        assert!(spec.negatives.is_empty());
        assert_eq!(spec.author, DEFAULT_AUTHOR);
        assert_eq!(spec.website, DEFAULT_WEBSITE);
        assert_eq!(spec.target_false_accepts_per_hour, 0.5);
        assert_eq!(spec.settings, default_settings());
    }

    #[test]
    fn hey_runa_as_webber_asks_for_it() {
        let spec = parsed(json!({
            "phrase": "hey runa",
            "languages": ["en", "nl"],
            "spellings": {"en": ["hey roona"], "nl": ["hej roena", "hey roena"]},
            "negatives": ["hey luna", "hey ruth", "ruin a", "hey rune", "runner", "hey google",
                          "hey jarvis", "okay nabu"],
            "author": "Webber Takken",
            "targetFalseAcceptsPerHour": 0.5
        }))
        .unwrap();
        assert_eq!(spec.languages, [Language::En, Language::Nl]);
        assert_eq!(spec.spellings[&Language::Nl], ["hej roena", "hey roena"]);
        assert_eq!(spec.negatives.len(), 8);
        assert_eq!(spec.author, "Webber Takken");
    }

    #[test]
    fn a_language_without_spellings_reads_the_phrase() {
        let spec =
            parsed(json!({"phrase": "hey runa", "languages": ["nl", "de"], "spellings": {"nl": ["hej roena"]}}))
                .unwrap();
        assert_eq!(spec.spellings[&Language::De], ["hey runa"]);
    }

    #[test]
    fn every_bad_field_is_refused_by_name() {
        let cases = [
            (json!({}), "phrase"),
            (json!({"phrase": "x"}), "phrase must be 2 to 64"),
            (json!({"phrase": "hey <b>"}), "letters, spaces"),
            (json!({"phrase": "hey runa", "colour": 1}), "colour"),
            (json!({"phrase": "hey runa", "name": "Hey"}), "name must be"),
            (
                json!({"phrase": "hey runa", "languages": []}),
                "at least one",
            ),
            (
                json!({"phrase": "hey runa", "languages": ["xx"]}),
                "\"xx\" is not one of en, nl, de, fr",
            ),
            (
                json!({"phrase": "hey runa", "languages": ["en", "en"]}),
                "named twice",
            ),
            (
                json!({"phrase": "hey runa", "spellings": {"nl": ["hej roena"]}}),
                "spellings.nl",
            ),
            (
                json!({"phrase": "hey runa", "spellings": {"en": []}}),
                "spellings.en must hold",
            ),
            (
                json!({"phrase": "hey runa", "spellings": {"en": ["1"]}}),
                "spellings.en",
            ),
            (
                json!({"phrase": "hey runa", "negatives": ["hey runa"]}),
                "the phrase itself",
            ),
            (
                json!({"phrase": "hey runa", "spellings": {"en": ["hey roona"]}, "negatives": ["Hey Roona"]}),
                "spellings",
            ),
            (
                json!({"phrase": "hey runa", "negatives": ["hey luna", "hey luna"]}),
                "named twice",
            ),
            (
                json!({"phrase": "hey runa", "negatives": vec!["hey luna"; 33]}),
                "at most 32",
            ),
            (json!({"phrase": "hey runa", "author": ""}), "author"),
            (
                json!({"phrase": "hey runa", "author": "a\u{7}b"}),
                "control",
            ),
            (
                json!({"phrase": "hey runa", "website": "ftp://x"}),
                "website",
            ),
            (
                json!({"phrase": "hey runa", "targetFalseAcceptsPerHour": 0}),
                "targetFalseAcceptsPerHour",
            ),
            (
                json!({"phrase": "hey runa", "targetFalseAcceptsPerHour": 11}),
                "targetFalseAcceptsPerHour",
            ),
            (
                json!({"phrase": "hey runa", "settings": {"samples": {"batchSize": 0}}}),
                "samples.batchSize",
            ),
            (
                json!({"phrase": "hey runa", "settings": {"prosody": {"en": null}}}),
                "settings.prosody.en",
            ),
            (
                json!({"phrase": "éé ôô"}),
                "name: the phrase has no letters",
            ),
            (
                json!({"phrase": "hey runa", "negatives": ["éé ôô"]}),
                "needs a letter a-z",
            ),
            (
                json!({"phrase": "hey runa", "negatives": ["hey luna", "hey-luna"]}),
                "share the name hey_luna",
            ),
        ];
        for (body, needle) in cases {
            let message = refused(body.clone());
            assert!(message.contains(needle), "{body}: {message}");
        }
    }

    #[test]
    fn recording_sets_are_named_safely_once_each() {
        assert!(parsed(json!({"phrase": "hey runa"}))
            .unwrap()
            .voice_samples
            .is_empty());
        let spec =
            parsed(json!({"phrase": "hey runa", "voiceSamples": ["2026-10-26", "s2"]})).unwrap();
        assert_eq!(spec.voice_samples, ["2026-10-26", "s2"]);
        assert!(
            refused(json!({"phrase": "hey runa", "voiceSamples": ["../x"]}))
                .contains("voiceSamples")
        );
        assert!(
            refused(json!({"phrase": "hey runa", "voiceSamples": ["s1", "s1"]})).contains("twice")
        );
        assert!(
            refused(json!({"phrase": "hey runa", "voiceSamples": vec!["s"; 9]}))
                .contains("at most 8")
        );
    }

    #[test]
    fn a_given_display_name_and_website_are_kept() {
        let spec = parsed(json!({"phrase": "hey runa", "displayName": "Hey Rúna",
            "website": "https://example.org/runa", "name": "runa_2"}))
        .unwrap();
        assert_eq!(spec.display_name, "Hey Rúna");
        assert_eq!(spec.website, "https://example.org/runa");
        assert_eq!(spec.name, "runa_2");
    }

    #[test]
    fn a_slug_keeps_ascii_letters_and_digits() {
        assert_eq!(slug("Hey, Runa!"), "hey_runa");
        assert_eq!(slug("okay nabu 2"), "okay_nabu_2");
        assert_eq!(slug(&"a b ".repeat(40)).len(), 47);
    }
}

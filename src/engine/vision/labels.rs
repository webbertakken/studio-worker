//! The class names of a vision model, by output index, read from its `labels` file:
//!
//! - a Hugging Face `config.json` (its `id2label` map);
//! - a JSON array of names;
//! - text, one class per line.  A line of seven `;`-separated fields is a SpeciesNet label
//!   (`id;class;order;family;genus;species;common name`) and carries its taxonomy.

use anyhow::{bail, Context, Result};
use serde::Serialize;

/// One class a model answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    /// What the class is called: the common name when the label has one.
    pub name: String,
    /// Where the class sits in the tree of life, when its label says.
    pub taxonomy: Option<Taxonomy>,
}

impl Label {
    pub fn plain(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            taxonomy: None,
        }
    }
}

/// The ranks a label names; a rank the label leaves empty is absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Taxonomy {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genus: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub species: Option<String>,
    /// Genus and species epithet together (`Turdus merula`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scientific_name: Option<String>,
}

/// Parse a labels file; refused when it names no class or skips an index.
pub fn parse(text: &str) -> Result<Vec<Label>> {
    let trimmed = text.trim_start_matches('\u{feff}').trim();
    let labels = if trimmed.starts_with('{') {
        from_hf_config(trimmed)?
    } else if trimmed.starts_with('[') {
        let names: Vec<String> =
            serde_json::from_str(trimmed).context("reading a JSON array of labels")?;
        names.into_iter().map(Label::plain).collect()
    } else {
        from_lines(trimmed)?
    };
    if labels.is_empty() {
        bail!("the labels file names no class");
    }
    Ok(labels)
}

/// A Hugging Face `config.json`: `id2label` maps every index from 0 to a name.
fn from_hf_config(text: &str) -> Result<Vec<Label>> {
    let config: serde_json::Value =
        serde_json::from_str(text).context("reading the labels config as JSON")?;
    let Some(map) = config.get("id2label").and_then(|m| m.as_object()) else {
        bail!("the labels config has no id2label map");
    };
    let mut names = vec![None; map.len()];
    for (key, value) in map {
        let index: usize = key
            .parse()
            .with_context(|| format!("id2label key {key:?} is not an index"))?;
        let Some(slot) = names.get_mut(index) else {
            bail!(
                "id2label skips an index: {index} is past {} labels",
                map.len()
            );
        };
        let Some(name) = value.as_str() else {
            bail!("id2label {index} is not a string");
        };
        *slot = Some(name.to_string());
    }
    names
        .into_iter()
        .enumerate()
        .map(|(i, n)| {
            n.map(Label::plain)
                .with_context(|| format!("id2label lacks {i}"))
        })
        .collect()
}

/// One label per line; blank lines at the end are ignored, a blank line between labels is
/// refused (every label after it would answer for the wrong class).
fn from_lines(text: &str) -> Result<Vec<Label>> {
    text.lines()
        .enumerate()
        .map(|(i, line)| {
            let line = line.trim();
            if line.is_empty() {
                bail!("labels line {} is empty", i + 1);
            }
            Ok(from_line(line))
        })
        .collect()
}

fn from_line(line: &str) -> Label {
    let fields: Vec<&str> = line.split(';').map(str::trim).collect();
    let [_, class, order, family, genus, species, common] = fields[..] else {
        return Label::plain(line);
    };
    let rank = |s: &str| (!s.is_empty()).then(|| s.to_string());
    let taxonomy = Taxonomy {
        class: rank(class),
        order: rank(order),
        family: rank(family),
        genus: rank(genus),
        species: rank(species),
        scientific_name: (!genus.is_empty() && !species.is_empty())
            .then(|| format!("{} {species}", capitalised(genus))),
    };
    let name = [common, species, genus, family, order, class]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or(line)
        .to_string();
    Label {
        name,
        taxonomy: Some(taxonomy),
    }
}

/// `word` with its first letter upper case.
fn capitalised(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hugging_face_config_gives_its_labels_in_index_order() {
        let config = r#"{"id2label": {"1": "ABBOTTS BOOBY", "0": "ABBOTTS BABBLER", "10": "ALBERTS TOWHEE",
            "2": "c", "3": "d", "4": "e", "5": "f", "6": "g", "7": "h", "8": "i", "9": "j"}}"#;
        let labels = parse(config).unwrap();
        assert_eq!(labels.len(), 11);
        assert_eq!(labels[0], Label::plain("ABBOTTS BABBLER"));
        assert_eq!(labels[1].name, "ABBOTTS BOOBY");
        assert_eq!(labels[10].name, "ALBERTS TOWHEE");
    }

    #[test]
    fn a_config_that_skips_an_index_is_refused() {
        let err = parse(r#"{"id2label": {"0": "a", "2": "c"}}"#).unwrap_err();
        assert!(err.to_string().contains("skips an index"), "{err}");
    }

    #[test]
    fn a_config_without_id2label_is_refused() {
        let err = parse(r#"{"labels": []}"#).unwrap_err();
        assert!(err.to_string().contains("no id2label"), "{err}");
    }

    #[test]
    fn a_config_key_that_is_no_index_is_refused() {
        let err = parse(r#"{"id2label": {"zero": "a"}}"#).unwrap_err();
        assert!(err.to_string().contains("not an index"), "{err}");
    }

    #[test]
    fn a_config_label_that_is_no_string_is_refused() {
        let err = parse(r#"{"id2label": {"0": 7}}"#).unwrap_err();
        assert!(err.to_string().contains("not a string"), "{err}");
    }

    #[test]
    fn a_json_array_gives_its_labels() {
        assert_eq!(
            parse(r#"["animal", "person", "vehicle"]"#).unwrap(),
            vec![
                Label::plain("animal"),
                Label::plain("person"),
                Label::plain("vehicle")
            ]
        );
    }

    #[test]
    fn plain_lines_are_one_label_each() {
        let labels = parse("\u{feff}blackbird\nrobin\r\nwren\n\n").unwrap();
        let names: Vec<&str> = labels.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["blackbird", "robin", "wren"]);
        assert!(labels.iter().all(|l| l.taxonomy.is_none()));
    }

    #[test]
    fn a_blank_line_between_labels_is_refused() {
        let err = parse("blackbird\n\nrobin").unwrap_err();
        assert!(err.to_string().contains("line 2 is empty"), "{err}");
    }

    #[test]
    fn an_empty_file_is_refused() {
        assert!(parse("  \n").is_err());
        assert!(parse("[]").is_err());
    }

    #[test]
    fn a_speciesnet_line_carries_its_taxonomy() {
        let label = parse("e7f5-uuid;aves;passeriformes;turdidae;turdus;merula;eurasian blackbird")
            .unwrap()
            .remove(0);
        assert_eq!(label.name, "eurasian blackbird");
        let taxonomy = label.taxonomy.unwrap();
        assert_eq!(taxonomy.class.as_deref(), Some("aves"));
        assert_eq!(taxonomy.order.as_deref(), Some("passeriformes"));
        assert_eq!(taxonomy.family.as_deref(), Some("turdidae"));
        assert_eq!(taxonomy.genus.as_deref(), Some("turdus"));
        assert_eq!(taxonomy.species.as_deref(), Some("merula"));
        assert_eq!(taxonomy.scientific_name.as_deref(), Some("Turdus merula"));
    }

    #[test]
    fn a_speciesnet_line_above_species_rank_names_its_lowest_rank() {
        let label = parse("u;aves;;;;;").unwrap().remove(0);
        assert_eq!(label.name, "aves");
        let taxonomy = label.taxonomy.unwrap();
        assert_eq!(taxonomy.order, None);
        assert_eq!(taxonomy.scientific_name, None);
    }

    #[test]
    fn a_speciesnet_blank_line_keeps_the_raw_label() {
        let label = parse("u;;;;;;").unwrap().remove(0);
        assert_eq!(label.name, "u;;;;;;");
    }

    #[test]
    fn taxonomy_serialises_only_the_ranks_it_knows() {
        let json = serde_json::to_value(Taxonomy {
            class: Some("aves".into()),
            scientific_name: Some("Turdus merula".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({"class": "aves", "scientificName": "Turdus merula"})
        );
    }
}

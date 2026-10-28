//! The training report: what was measured, which cutoff was chosen and why, and the whole recipe
//! (`docs/engines/wake-word.md`, "Report").  Built purely from the training's facts; written as
//! `report.json` and rendered as `report.md`.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::cutoff::{accepted_share, poisson_upper_95, Choice, Evaluation, Split};
use super::own_voice::{OwnVoice, OwnVoiceRecall};
use super::recipe::{Download, ENVIRONMENT};
use super::request::{slug, TrainingSpec};
use super::settings::Settings;
use crate::engine::torch::recipe::Source;
use crate::voice_samples::TakeFile;

/// Grid points shown on each side of the chosen cutoff in the report's table.
const TABLE_REACH: usize = 5;

/// How a voice reads a text, as the samples stage reported it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Phonemes {
    pub language: String,
    pub text: String,
    pub role: String,
    pub phonemes: String,
}

/// One stage's wall time and device memory.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StageTiming {
    pub stage: String,
    pub seconds: f64,
    pub device_memory_peak_mib: u64,
    pub host_memory_peak_mib: u64,
    pub anon_memory_peak_mib: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AmbientResult {
    pub name: String,
    pub hours: f64,
    pub false_accepts: u64,
    pub per_hour: f64,
    /// The one-sided 95% upper bound on the rate (Poisson).
    pub per_hour_upper_95: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelledRate {
    pub label: String,
    pub clips: usize,
    pub rate: f64,
}

/// How the chosen cutoff does on one split.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SplitResult {
    pub recall: f64,
    pub recall_by_language: Vec<LabelledRate>,
    pub ambient: Vec<AmbientResult>,
    pub pooled: AmbientResult,
    /// Each near miss's share of held-out clips that wake the model.
    pub near_miss_accepts: Vec<LabelledRate>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CutoffRow {
    pub cutoff: f64,
    pub validation_false_accepts_per_hour: f64,
    pub testing_false_accepts_per_hour: f64,
    pub validation_recall: f64,
    pub testing_recall: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentFacts {
    pub python: &'static str,
    pub torch: &'static [&'static str],
    pub torch_index: &'static str,
    pub packages: &'static [&'static str],
    /// `uv pip freeze` of the built environment.
    pub freeze: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipeFacts {
    pub environment: EnvironmentFacts,
    pub sources: Vec<Source>,
    pub datasets: Vec<Download>,
    pub generators: Vec<Download>,
    pub settings: Settings,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelResult {
    pub file: String,
    pub bytes: u64,
    pub sha256: String,
    pub tensor_arena_min_bytes: u64,
    pub tensor_arena_size: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub id: String,
    pub name: String,
    pub phrase: String,
    pub display_name: String,
    pub languages: Vec<&'static str>,
    pub spellings: BTreeMap<&'static str, Vec<String>>,
    pub negatives: Vec<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub wall_seconds: f64,
    pub choice: Choice,
    pub validation: SplitResult,
    pub testing: SplitResult,
    pub table: Vec<CutoffRow>,
    pub sliding_window: u32,
    pub cooldown_slices: u32,
    pub phonemes: Vec<Phonemes>,
    pub model: ModelResult,
    pub stages: Vec<StageTiming>,
    pub gpu: Option<String>,
    pub recipe: RecipeFacts,
    pub caveats: Vec<String>,
    /// The operator's own takes, when the training named recording sets.
    pub own_voice: Option<OwnVoiceReport>,
}

/// What a training learned from, and measured on, the operator's own takes.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnVoiceReport {
    pub sets: Vec<String>,
    pub trained_positive: usize,
    pub near_miss: usize,
    pub speech: usize,
    /// On the held-out takes, at the cutoff.
    pub recall: OwnVoiceRecall,
    /// Every take used (set, kind, number, phrase, sha256): never its audio or its path.
    pub takes: Vec<TakeFile>,
}

/// Everything a report is built from.
pub struct Facts<'a> {
    pub id: &'a str,
    pub spec: &'a TrainingSpec,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub evaluation: &'a Evaluation,
    pub choice: &'a Choice,
    pub phonemes: Vec<Phonemes>,
    pub stages: Vec<StageTiming>,
    pub freeze: Vec<String>,
    pub gpu: Option<String>,
    pub datasets: Vec<Download>,
    pub generators: Vec<Download>,
    pub tensor_arena_size: u64,
    pub own_voice: Option<&'a OwnVoice>,
}

fn ambient_result(name: &str, hours: f64, count: u64) -> AmbientResult {
    let rate = |count: f64| if hours > 0.0 { count / hours } else { 0.0 };
    AmbientResult {
        name: name.into(),
        hours,
        false_accepts: count,
        per_hour: rate(count as f64),
        per_hour_upper_95: rate(poisson_upper_95(count)),
    }
}

fn split_result(
    evaluation: &Evaluation,
    spec: &TrainingSpec,
    split: Split,
    index: usize,
) -> SplitResult {
    let cutoff = evaluation.cutoffs[index];
    let ambient: Vec<AmbientResult> = evaluation
        .ambient
        .iter()
        .filter(|a| a.split == split)
        .map(|a| ambient_result(&a.name, a.hours, a.false_accepts[index]))
        .collect();
    let (count, hours) = evaluation.false_accepts(split, index);
    let near_miss_text: BTreeMap<String, &String> = spec
        .negatives
        .iter()
        .map(|text| (slug(text), text))
        .collect();
    let rates = |list: &[super::cutoff::Scores], rename: &dyn Fn(&str) -> String| {
        list.iter()
            .filter(|s| s.split == split)
            .map(|s| LabelledRate {
                label: rename(&s.label),
                clips: s.scores.len(),
                rate: accepted_share(&s.scores, cutoff),
            })
            .collect::<Vec<_>>()
    };
    SplitResult {
        recall: evaluation.recall(split, cutoff),
        recall_by_language: rates(&evaluation.positive, &|l| l.to_string()),
        ambient,
        pooled: ambient_result("pooled", hours, count),
        near_miss_accepts: rates(&evaluation.near_miss, &|l| {
            near_miss_text
                .get(l)
                .map(|t| t.to_string())
                .unwrap_or_else(|| l.to_string())
        }),
    }
}

fn table(evaluation: &Evaluation, chosen: usize) -> Vec<CutoffRow> {
    let low = chosen.saturating_sub(TABLE_REACH);
    let high = (chosen + TABLE_REACH).min(evaluation.cutoffs.len() - 1);
    (low..=high)
        .map(|i| {
            let cutoff = evaluation.cutoffs[i];
            let rate = |split| {
                let (count, hours) = evaluation.false_accepts(split, i);
                count as f64 / hours
            };
            CutoffRow {
                cutoff,
                validation_false_accepts_per_hour: rate(Split::Validation),
                testing_false_accepts_per_hour: rate(Split::Testing),
                validation_recall: evaluation.recall(Split::Validation, cutoff),
                testing_recall: evaluation.recall(Split::Testing, cutoff),
            }
        })
        .collect()
}

pub fn build(facts: Facts<'_>) -> Report {
    let spec = facts.spec;
    let e = facts.evaluation;
    let index = facts.choice.index;
    Report {
        id: facts.id.into(),
        name: spec.name.clone(),
        phrase: spec.phrase.clone(),
        display_name: spec.display_name.clone(),
        languages: spec.languages.iter().map(|l| l.code()).collect(),
        spellings: spec
            .spellings
            .iter()
            .map(|(l, s)| (l.code(), s.clone()))
            .collect(),
        negatives: spec.negatives.clone(),
        started_at: facts.started_at,
        finished_at: facts.finished_at,
        wall_seconds: (facts.finished_at - facts.started_at).num_milliseconds() as f64 / 1000.0,
        choice: facts.choice.clone(),
        validation: split_result(e, spec, Split::Validation, index),
        testing: split_result(e, spec, Split::Testing, index),
        table: table(e, index),
        sliding_window: e.sliding_window,
        cooldown_slices: e.cooldown_slices,
        phonemes: facts.phonemes,
        model: ModelResult {
            file: format!("{}.tflite", spec.name),
            bytes: e.model.bytes,
            sha256: e.model.sha256.clone(),
            tensor_arena_min_bytes: e.model.tensor_arena_min_bytes,
            tensor_arena_size: facts.tensor_arena_size,
        },
        stages: facts.stages,
        gpu: facts.gpu,
        recipe: RecipeFacts {
            environment: EnvironmentFacts {
                python: ENVIRONMENT.python,
                torch: ENVIRONMENT.torch,
                torch_index: ENVIRONMENT.torch_index,
                packages: ENVIRONMENT.packages,
                freeze: facts.freeze,
            },
            sources: ENVIRONMENT.sources.to_vec(),
            datasets: facts.datasets,
            generators: facts.generators,
            settings: spec.settings.clone(),
        },
        caveats: vec![
            "Recall is measured on held-out synthetic speech from the same generators: real voices, \
             accents and microphones will do worse."
                .into(),
            format!(
                "False accepts per hour rest on {:.1} hours of testing audio; the upper bounds say \
                 how loosely that pins the rate.",
                e.false_accepts(Split::Testing, index).1
            ),
            "The negative feature sets are CC BY-NC 4.0: the model is fit for personal, \
             non-commercial use."
                .into(),
        ],
        own_voice: facts.own_voice.map(|own| {
            let scores = e.own_voice.as_ref().map(|o| o.held_out.as_slice()).unwrap_or(&[]);
            OwnVoiceReport {
                sets: own.sets.clone(),
                trained_positive: own.train_positive.len(),
                near_miss: own.near_miss.len(),
                speech: own.speech.len(),
                recall: OwnVoiceRecall::at(scores, facts.choice.cutoff),
                takes: own
                    .train_positive
                    .iter()
                    .chain(&own.held_out)
                    .chain(&own.near_miss)
                    .chain(&own.speech)
                    .cloned()
                    .collect(),
            }
        }),
    }
}

fn percent(rate: f64) -> String {
    format!("{:.1}%", rate * 100.0)
}

fn split_markdown(out: &mut String, title: &str, result: &SplitResult) {
    let _ = writeln!(out, "### {title}\n");
    let _ = writeln!(out, "- Recall: {}", percent(result.recall));
    for language in &result.recall_by_language {
        let _ = writeln!(
            out,
            "  - `{}`: {} of {} clips",
            language.label,
            percent(language.rate),
            language.clips
        );
    }
    let _ = writeln!(
        out,
        "- False accepts: {} in {:.2} h, {:.3} per hour (95% upper bound {:.3})",
        result.pooled.false_accepts,
        result.pooled.hours,
        result.pooled.per_hour,
        result.pooled.per_hour_upper_95
    );
    for set in &result.ambient {
        let _ = writeln!(
            out,
            "  - {}: {} in {:.2} h, {:.3} per hour (95% upper bound {:.3})",
            set.name, set.false_accepts, set.hours, set.per_hour, set.per_hour_upper_95
        );
    }
    if !result.near_miss_accepts.is_empty() {
        let _ = writeln!(out, "- Near misses that wake it:");
        for near in &result.near_miss_accepts {
            let _ = writeln!(
                out,
                "  - {}: {} of {} clips",
                near.label,
                percent(near.rate),
                near.clips
            );
        }
    }
    out.push('\n');
}

/// The report for people.
pub fn markdown(report: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Wake word \"{}\"\n", report.display_name);
    let _ = writeln!(
        out,
        "Phrase `{}`, files `{}`, languages {}; training `{}`, {} to {} ({:.0} min).\n",
        report.phrase,
        report.name,
        report.languages.join(", "),
        report.id,
        report.started_at.to_rfc3339(),
        report.finished_at.to_rfc3339(),
        report.wall_seconds / 60.0
    );
    let _ = writeln!(out, "## Result\n");
    let _ = writeln!(
        out,
        "- Cutoff: **{}** (`probability_cutoff`), sliding window {} slices.",
        report.choice.cutoff, report.sliding_window
    );
    let _ = writeln!(
        out,
        "- Target: at most {} false accepts per hour: **{}**.",
        report.choice.target_false_accepts_per_hour,
        if report.choice.target_met {
            "met"
        } else {
            "not met"
        }
    );
    let _ = writeln!(out, "- Why: {}.\n", report.choice.why);
    split_markdown(&mut out, "Testing (held out; reported)", &report.testing);
    split_markdown(
        &mut out,
        "Validation (chose the cutoff)",
        &report.validation,
    );

    let _ = writeln!(out, "### Around the cutoff\n");
    let _ = writeln!(
        out,
        "| Cutoff | Validation FA/h | Testing FA/h | Validation recall | Testing recall |"
    );
    let _ = writeln!(out, "| --- | --- | --- | --- | --- |");
    for row in &report.table {
        let mark = if row.cutoff == report.choice.cutoff {
            " ←"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "| {}{} | {:.3} | {:.3} | {} | {} |",
            row.cutoff,
            mark,
            row.validation_false_accepts_per_hour,
            row.testing_false_accepts_per_hour,
            percent(row.validation_recall),
            percent(row.testing_recall)
        );
    }

    if let Some(own) = &report.own_voice {
        let _ = writeln!(out, "\n## Your own voice\n");
        let r = own.recall;
        let _ = writeln!(
            out,
            "At the cutoff the model heard {} of {} held-out takes ({}), from set {}; {} positive, {} near-miss and {} speech takes trained.",
            r.accepted,
            r.held_out,
            percent(r.recall),
            own.sets.join(", "),
            own.trained_positive,
            own.near_miss,
            own.speech
        );
        let _ = writeln!(out, "\nThe takes it used, by sha256, are in `report.json`.");
    }

    let _ = writeln!(out, "\n## How the voices read it\n");
    let _ = writeln!(out, "| Language | Role | Text | Phonemes |");
    let _ = writeln!(out, "| --- | --- | --- | --- |");
    for p in &report.phonemes {
        let _ = writeln!(
            out,
            "| {} | {} | {} | /{}/ |",
            p.language, p.role, p.text, p.phonemes
        );
    }

    let _ = writeln!(out, "\n## Model\n");
    let m = &report.model;
    let _ = writeln!(
        out,
        "`{}`: {} bytes, sha256 `{}`; TensorFlow Lite Micro runs it in {} bytes, the manifest asks {}.\n",
        m.file, m.bytes, m.sha256, m.tensor_arena_min_bytes, m.tensor_arena_size
    );

    let _ = writeln!(out, "## Run\n");
    if let Some(gpu) = &report.gpu {
        let _ = writeln!(out, "GPU: {gpu}.\n");
    }
    let _ = writeln!(
        out,
        "| Stage | Seconds | Device memory peak (MiB) | Host memory peak (MiB) |"
    );
    let _ = writeln!(out, "| --- | --- | --- | --- |");
    for s in &report.stages {
        let _ = writeln!(
            out,
            "| {} | {:.0} | {} | {} | {} |",
            s.stage,
            s.seconds,
            s.device_memory_peak_mib,
            s.host_memory_peak_mib,
            s.anon_memory_peak_mib
        );
    }

    let _ = writeln!(out, "\n## Recipe\n");
    let env = &report.recipe.environment;
    let _ = writeln!(
        out,
        "Python {}, {} from {}, {}.\n",
        env.python,
        env.torch.join(", "),
        env.torch_index,
        env.packages.join(", ")
    );
    for source in &report.recipe.sources {
        let _ = writeln!(
            out,
            "- {} `{}` at `{}`",
            source.name, source.repo, source.rev
        );
    }
    for d in report
        .recipe
        .datasets
        .iter()
        .chain(&report.recipe.generators)
    {
        let _ = writeln!(
            out,
            "- {} `{}` ({}, {} bytes, sha256 `{}`)",
            d.name, d.url, d.licence, d.bytes, d.sha256
        );
    }
    let _ = writeln!(
        out,
        "\nSettings (seed {}), and the environment's full `pip freeze`, are in `report.json`.\n",
        report.recipe.settings.seed
    );
    let _ = writeln!(out, "## Caveats\n");
    for caveat in &report.caveats {
        let _ = writeln!(out, "- {caveat}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wake_word::cutoff::{choose, tests::evaluation};
    use crate::wake_word::recipe::{datasets, Language};
    use crate::wake_word::request;

    fn report() -> Report {
        report_with(evaluation(), None)
    }

    fn report_with(e: Evaluation, own_voice: Option<&OwnVoice>) -> Report {
        let spec = request::parse(
            br#"{"phrase":"hey runa","languages":["en","nl"],"negatives":["hey luna"]}"#,
        )
        .unwrap();
        let choice = choose(&e, 0.5);
        let started = "2026-10-25T10:00:00Z".parse().unwrap();
        build(Facts {
            id: "local-1",
            spec: &spec,
            started_at: started,
            finished_at: started + chrono::Duration::seconds(5400),
            evaluation: &e,
            choice: &choice,
            phonemes: vec![Phonemes {
                language: "en".into(),
                text: "hey roona".into(),
                role: "positive".into(),
                phonemes: "hˈeɪ ɹˈuːnə".into(),
            }],
            stages: vec![StageTiming {
                stage: "train".into(),
                seconds: 2400.0,
                device_memory_peak_mib: 4498,
                host_memory_peak_mib: 21000,
                anon_memory_peak_mib: 7000,
            }],
            freeze: vec!["torch==2.14.1+cu126".into()],
            gpu: Some("NVIDIA GeForce RTX 4090, 24564 MiB".into()),
            datasets: datasets(),
            generators: vec![Language::En.generator()],
            tensor_arena_size: 32_768,
            own_voice,
        })
    }

    #[test]
    fn own_voice_recall_is_reported_apart_with_its_takes() {
        use crate::voice_samples::{Kind, TakeFile};
        let take = |kind, number| TakeFile {
            set: "s1".into(),
            kind,
            number,
            phrase: None,
            path: "/never/in/the/report.wav".into(),
            sha256: format!("{number:064}"),
        };
        let takes: Vec<TakeFile> = (1..=8)
            .map(|n| take(Kind::Positive, n))
            .chain([take(Kind::Speech, 1)])
            .collect();
        let own = OwnVoice::split(&["s1".into()], &takes).unwrap();
        let mut e = evaluation();
        e.own_voice = Some(crate::wake_word::cutoff::OwnVoiceScores {
            held_out: vec![0.95, 0.4],
        });
        let r = report_with(e, Some(&own));
        let own_report = r.own_voice.as_ref().unwrap();
        assert_eq!(
            own_report.recall,
            OwnVoiceRecall {
                recall: 0.5,
                accepted: 1,
                held_out: 2
            }
        );
        assert_eq!(
            (
                own_report.trained_positive,
                own_report.near_miss,
                own_report.speech
            ),
            (6, 0, 1)
        );
        assert_eq!(own_report.takes.len(), 9);
        let json = serde_json::to_string(&r).unwrap();
        assert!(
            !json.contains("/never/in/the/report.wav"),
            "no path to a take"
        );
        let md = markdown(&r);
        assert!(md.contains("## Your own voice"), "{md}");
        assert!(md.contains("heard 1 of 2 held-out takes (50.0%)"), "{md}");
        assert!(report().own_voice.is_none());
        assert!(!markdown(&report()).contains("Your own voice"));
    }

    #[test]
    fn the_report_carries_the_choice_and_both_splits_at_it() {
        let r = report();
        assert_eq!(r.choice.cutoff, 0.7);
        assert_eq!(r.wall_seconds, 5400.0);
        assert_eq!(r.testing.recall, 2.0 / 3.0);
        let dipco = &r.testing.ambient[0];
        assert_eq!((dipco.name.as_str(), dipco.false_accepts), ("dipco", 0));
        assert!((dipco.per_hour_upper_95 - 2.9957 / 5.0).abs() < 0.01);
        assert_eq!(r.testing.near_miss_accepts[0].label, "hey luna");
        assert_eq!(r.testing.near_miss_accepts[0].rate, 0.5);
        assert_eq!(r.validation.recall_by_language.len(), 2);
        assert_eq!(r.table.first().unwrap().cutoff, 0.2);
        assert_eq!(r.table.last().unwrap().cutoff, 1.0);
        assert_eq!(r.recipe.environment.python, "3.12.15");
        assert_eq!(r.recipe.sources.len(), 2);
    }

    #[test]
    fn the_markdown_reads_the_result_first() {
        let md = markdown(&report());
        assert!(md.starts_with("# Wake word \"Hey Runa\""), "{md}");
        assert!(md.contains("- Cutoff: **0.7**"), "{md}");
        assert!(md.contains("**met**"), "{md}");
        assert!(md.contains("| 0.7 ← |"), "{md}");
        assert!(md.contains("/hˈeɪ ɹˈuːnə/"), "{md}");
        assert!(md.contains("  - hey luna: 50.0% of 2 clips"), "{md}");
        assert!(md.contains("GPU: NVIDIA GeForce RTX 4090"), "{md}");
        assert!(
            md.contains("microWakeWord `https://github.com/kahrendt/microWakeWord.git`"),
            "{md}"
        );
        assert!(
            md.contains("| train | 2400 | 4498 | 21000 | 7000 |"),
            "{md}"
        );
        let json = serde_json::to_value(report()).unwrap();
        assert_eq!(json["recipe"]["datasets"][0]["role"], "negativeFeatures");
        assert_eq!(
            json["recipe"]["environment"]["freeze"][0],
            "torch==2.14.1+cu126"
        );
    }
}

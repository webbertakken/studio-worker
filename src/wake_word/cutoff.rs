//! Choosing the cutoff from what the evaluate stage measured (`docs/engines/wake-word.md`,
//! "Choosing the cutoff").  Pure: measurements in, a choice and its numbers out.

use serde::{Deserialize, Serialize};

/// Which held-out data a measurement is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Split {
    /// Chooses the cutoff.
    Validation,
    /// Reports how the chosen cutoff does.
    Testing,
}

/// One ambient set: its hours and its false accepts at every cutoff of the grid.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AmbientSet {
    pub name: String,
    pub split: Split,
    pub hours: f64,
    pub false_accepts: Vec<u64>,
}

/// Each held-out clip's best smoothed score, for a language's positives or a near miss.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Scores {
    #[serde(alias = "language", alias = "phrase")]
    pub label: String,
    pub split: Split,
    pub scores: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelFacts {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub tensor_arena_min_bytes: u64,
}

/// `evaluation.json`, as the evaluate stage writes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Evaluation {
    pub cutoffs: Vec<f64>,
    pub sliding_window: u32,
    pub cooldown_slices: u32,
    pub stride: u32,
    pub ambient: Vec<AmbientSet>,
    pub positive: Vec<Scores>,
    pub near_miss: Vec<Scores>,
    pub model: ModelFacts,
}

impl Evaluation {
    /// Whether the measurements fit together: one count per cutoff, hours and clips to measure.
    pub fn check(&self) -> Result<(), String> {
        if self.cutoffs.is_empty() || self.cutoffs.windows(2).any(|w| w[0] >= w[1]) {
            return Err("the cutoffs are not an ascending grid".into());
        }
        for split in [Split::Validation, Split::Testing] {
            if !self
                .ambient
                .iter()
                .any(|a| a.split == split && a.hours > 0.0)
            {
                return Err(format!("no {split:?} ambient audio was measured"));
            }
            if !self
                .positive
                .iter()
                .any(|p| p.split == split && !p.scores.is_empty())
            {
                return Err(format!("no {split:?} positives were measured"));
            }
        }
        if let Some(set) = self
            .ambient
            .iter()
            .find(|a| a.false_accepts.len() != self.cutoffs.len())
        {
            return Err(format!(
                "{} has {} counts for {} cutoffs",
                set.name,
                set.false_accepts.len(),
                self.cutoffs.len()
            ));
        }
        Ok(())
    }

    fn ambient_of(&self, split: Split) -> impl Iterator<Item = &AmbientSet> {
        self.ambient.iter().filter(move |a| a.split == split)
    }

    /// The pooled false accepts of `split` at grid point `index`: (count, hours).
    pub fn false_accepts(&self, split: Split, index: usize) -> (u64, f64) {
        self.ambient_of(split)
            .fold((0, 0.0), |(count, hours), set| {
                (count + set.false_accepts[index], hours + set.hours)
            })
    }

    /// Recall of every positive of `split` at `cutoff`.
    pub fn recall(&self, split: Split, cutoff: f64) -> f64 {
        let all: Vec<f64> = self
            .positive
            .iter()
            .filter(|p| p.split == split)
            .flat_map(|p| p.scores.iter().copied())
            .collect();
        accepted_share(&all, cutoff)
    }
}

/// The share of `scores` above `cutoff` (0 when there are none).
pub fn accepted_share(scores: &[f64], cutoff: f64) -> f64 {
    if scores.is_empty() {
        return 0.0;
    }
    scores.iter().filter(|s| **s > cutoff).count() as f64 / scores.len() as f64
}

/// The cutoff chosen, and the numbers that chose it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Choice {
    pub cutoff: f64,
    /// Its place on the grid.
    pub index: usize,
    pub target_false_accepts_per_hour: f64,
    pub target_met: bool,
    pub validation_false_accepts: u64,
    pub validation_hours: f64,
    pub validation_false_accepts_per_hour: f64,
    pub validation_recall: f64,
    /// The rule, in words, with its numbers.
    pub why: String,
}

/// The lowest grid cutoff at which validation's false accepts per hour, and those of every higher
/// cutoff, are at most `target`; else the lowest cutoff with the fewest.
pub fn choose(evaluation: &Evaluation, target: f64) -> Choice {
    let rates: Vec<(u64, f64, f64)> = (0..evaluation.cutoffs.len())
        .map(|i| {
            let (count, hours) = evaluation.false_accepts(Split::Validation, i);
            (count, hours, count as f64 / hours)
        })
        .collect();
    // From the top of the grid down, the lowest index from which every rate is within target.
    let mut lowest_within = None;
    for (i, (_, _, rate)) in rates.iter().enumerate().rev() {
        if *rate > target {
            break;
        }
        lowest_within = Some(i);
    }
    let (index, met) = match lowest_within {
        Some(i) => (i, true),
        None => {
            let fewest = rates
                .iter()
                .map(|(count, _, _)| *count)
                .min()
                .expect("a grid has cutoffs");
            let i = rates
                .iter()
                .position(|(count, _, _)| *count == fewest)
                .expect("the minimum is in the list");
            (i, false)
        }
    };
    let cutoff = evaluation.cutoffs[index];
    let (count, hours, rate) = rates[index];
    let recall = evaluation.recall(Split::Validation, cutoff);
    let sets: Vec<String> = evaluation
        .ambient_of(Split::Validation)
        .map(|a| format!("{} {:.1} h", a.name, a.hours))
        .collect();
    let why = if met {
        format!(
            "the lowest cutoff on the {step} grid at which the validation ambient audio ({sets}) \
             gives at most {target} false accepts per hour, at that cutoff and every higher one: \
             {count} in {hours:.1} h ({rate:.3} per hour); recall only falls as the cutoff rises, \
             so the lowest such cutoff keeps the most ({recall:.1}% of validation positives)",
            step = grid_step(evaluation),
            sets = sets.join(", "),
            recall = recall * 100.0,
        )
    } else {
        format!(
            "no cutoff on the grid keeps the validation ambient audio ({sets}) at or below {target} \
             false accepts per hour; this is the lowest cutoff with the fewest: {count} in \
             {hours:.1} h ({rate:.3} per hour), recall {recall:.1}%",
            sets = sets.join(", "),
            recall = recall * 100.0,
        )
    };
    Choice {
        cutoff,
        index,
        target_false_accepts_per_hour: target,
        target_met: met,
        validation_false_accepts: count,
        validation_hours: hours,
        validation_false_accepts_per_hour: rate,
        validation_recall: recall,
        why,
    }
}

fn grid_step(evaluation: &Evaluation) -> String {
    match evaluation.cutoffs.as_slice() {
        [first, second, ..] => format!("{}", ((second - first) * 1000.0).round() / 1000.0),
        _ => "single-point".into(),
    }
}

/// P(X <= k) for X ~ Poisson(mean).
fn poisson_cdf(k: u64, mean: f64) -> f64 {
    let mut term = (-mean).exp();
    let mut total = term;
    for i in 1..=k {
        term *= mean / i as f64;
        total += term;
    }
    total
}

/// The one-sided 95% upper bound on a Poisson mean after observing `k` events: the mean at which
/// seeing `k` or fewer has a 5% chance.
pub fn poisson_upper_95(k: u64) -> f64 {
    let (mut low, mut high) = (k as f64, k as f64 + 10.0 + 10.0 * (k as f64).sqrt());
    for _ in 0..200 {
        let middle = (low + high) / 2.0;
        if poisson_cdf(k, middle) > 0.05 {
            low = middle;
        } else {
            high = middle;
        }
    }
    (low + high) / 2.0
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A grid of 0.0..=1.0 by 0.1, validation and testing ambient counts falling with the cutoff.
    pub(crate) fn evaluation() -> Evaluation {
        let cutoffs: Vec<f64> = (0..=10).map(|i| i as f64 / 10.0).collect();
        let falling = |start: u64| -> Vec<u64> {
            (0..=10u64)
                .map(|i| start.saturating_sub(i * start / 7))
                .collect()
        };
        Evaluation {
            cutoffs,
            sliding_window: 5,
            cooldown_slices: 25,
            stride: 3,
            ambient: vec![
                AmbientSet {
                    name: "chime6".into(),
                    split: Split::Validation,
                    hours: 10.0,
                    false_accepts: falling(70),
                },
                AmbientSet {
                    name: "mls-dutch-dev".into(),
                    split: Split::Validation,
                    hours: 10.0,
                    false_accepts: falling(30),
                },
                AmbientSet {
                    name: "dipco".into(),
                    split: Split::Testing,
                    hours: 5.0,
                    false_accepts: falling(20),
                },
            ],
            positive: vec![
                Scores {
                    label: "en".into(),
                    split: Split::Validation,
                    scores: vec![0.95, 0.85, 0.75, 0.65, 0.2],
                },
                Scores {
                    label: "nl".into(),
                    split: Split::Validation,
                    scores: vec![0.99, 0.9, 0.5],
                },
                Scores {
                    label: "en".into(),
                    split: Split::Testing,
                    scores: vec![0.97, 0.8, 0.1],
                },
            ],
            near_miss: vec![Scores {
                label: "hey_luna".into(),
                split: Split::Testing,
                scores: vec![0.9, 0.3],
            }],
            model: ModelFacts {
                path: "m.tflite".into(),
                bytes: 62_304,
                sha256: "ab".repeat(32),
                tensor_arena_min_bytes: 25_609,
            },
        }
    }

    #[test]
    fn the_lowest_cutoff_within_target_is_chosen() {
        let e = evaluation();
        // pooled validation: 100 at 0.0, falling by ~14 per step: within 0.5/h over 20 h means <= 10
        let choice = choose(&e, 0.5);
        assert!(choice.target_met);
        let (count, hours) = e.false_accepts(Split::Validation, choice.index);
        assert!(count as f64 / hours <= 0.5);
        let (before, _) = e.false_accepts(Split::Validation, choice.index - 1);
        assert!(before as f64 / hours > 0.5, "the one below is over target");
        assert_eq!(choice.cutoff, e.cutoffs[choice.index]);
        assert!(
            choice.why.contains("lowest cutoff on the 0.1 grid"),
            "{}",
            choice.why
        );
        assert!(
            choice.why.contains("chime6 10.0 h, mls-dutch-dev 10.0 h"),
            "{}",
            choice.why
        );
    }

    #[test]
    fn a_rate_that_rises_again_higher_up_is_never_trusted() {
        let mut e = evaluation();
        // within target at 0.5, over at 0.6, within again above
        e.ambient[0].false_accepts = vec![90, 80, 70, 60, 50, 5, 30, 0, 0, 0, 0];
        e.ambient[1].false_accepts = vec![0; 11];
        let choice = choose(&e, 0.5);
        assert_eq!(choice.cutoff, 0.7);
    }

    #[test]
    fn when_nothing_meets_the_target_the_fewest_false_accepts_win() {
        let mut e = evaluation();
        e.ambient[0].false_accepts = vec![90, 80, 70, 60, 50, 40, 30, 20, 20, 20, 20];
        let choice = choose(&e, 0.5);
        assert!(!choice.target_met);
        assert_eq!(choice.cutoff, 0.7);
        assert!(
            choice.why.starts_with("no cutoff on the grid"),
            "{}",
            choice.why
        );
    }

    #[test]
    fn recall_counts_scores_strictly_above_the_cutoff() {
        let e = evaluation();
        assert_eq!(e.recall(Split::Validation, 0.9), 2.0 / 8.0);
        assert_eq!(e.recall(Split::Testing, 0.0), 1.0);
        assert_eq!(accepted_share(&[], 0.5), 0.0);
    }

    #[test]
    fn measurements_that_do_not_fit_are_named() {
        let mut e = evaluation();
        e.check().unwrap();
        e.ambient[2].false_accepts.pop();
        assert!(e
            .check()
            .unwrap_err()
            .contains("dipco has 10 counts for 11 cutoffs"));
        let mut e = evaluation();
        e.ambient.retain(|a| a.split != Split::Testing);
        assert!(e.check().unwrap_err().contains("Testing ambient"));
        let mut e = evaluation();
        e.positive.retain(|p| p.split != Split::Validation);
        assert!(e.check().unwrap_err().contains("Validation positives"));
        let mut e = evaluation();
        e.cutoffs.reverse();
        assert!(e.check().unwrap_err().contains("ascending"));
    }

    #[test]
    fn the_poisson_upper_bound_matches_the_textbook() {
        // Garwood one-sided 95%: 0 -> 2.996, 1 -> 4.744, 2 -> 6.296, 10 -> 17.0
        for (k, expected) in [(0, 2.9957), (1, 4.7439), (2, 6.2958), (10, 16.962)] {
            assert!(
                (poisson_upper_95(k) - expected).abs() < 0.01,
                "{k}: {}",
                poisson_upper_95(k)
            );
        }
    }

    #[test]
    fn evaluation_json_reads_as_the_stage_writes_it() {
        let json = r#"{"cutoffs":[0.0,0.5,1.0],"slidingWindow":5,"cooldownSlices":25,"stride":3,
          "ambient":[{"name":"dipco","split":"testing","hours":5.3,"falseAccepts":[3,1,0]}],
          "positive":[{"language":"en","split":"testing","scores":[0.9]}],
          "nearMiss":[{"phrase":"hey_luna","split":"validation","scores":[0.1]}],
          "model":{"path":"/m.tflite","bytes":1,"sha256":"x","tensorArenaMinBytes":2}}"#;
        let e: Evaluation = serde_json::from_str(json).unwrap();
        assert_eq!(e.positive[0].label, "en");
        assert_eq!(e.near_miss[0].label, "hey_luna");
        assert_eq!(e.ambient[0].false_accepts, [3, 1, 0]);
    }
}

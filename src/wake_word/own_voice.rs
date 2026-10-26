//! The operator's own takes in a training (`docs/engines/wake-word.md`, "Training with a set"):
//! which train, which are held out, and what holding them out measured.

use serde::{Deserialize, Serialize};

use crate::voice_samples::{Kind, TakeFile};

/// Every this-many-th positive (by take number) is held out.  Runa's spec.  Safe range 3 to 10.
pub const HOLD_OUT_EVERY: u32 = 4;
/// Positives a training needs, so at least one is held out.
pub const MIN_POSITIVES: usize = HOLD_OUT_EVERY as usize;

/// A training's takes, split as the spec says.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnVoice {
    pub sets: Vec<String>,
    pub train_positive: Vec<TakeFile>,
    pub held_out: Vec<TakeFile>,
    pub near_miss: Vec<TakeFile>,
    pub speech: Vec<TakeFile>,
}

impl OwnVoice {
    /// Split `takes`: positives numbered a multiple of [`HOLD_OUT_EVERY`] are held out.
    pub fn split(sets: &[String], takes: &[TakeFile]) -> Result<Self, String> {
        let mut own = OwnVoice {
            sets: sets.to_vec(),
            ..Default::default()
        };
        for take in takes {
            let list = match take.kind {
                Kind::Positive if take.number % HOLD_OUT_EVERY == 0 => &mut own.held_out,
                Kind::Positive => &mut own.train_positive,
                Kind::NearMiss => &mut own.near_miss,
                Kind::Speech => &mut own.speech,
            };
            list.push(take.clone());
        }
        let positives = own.train_positive.len() + own.held_out.len();
        if own.held_out.is_empty() || own.train_positive.is_empty() {
            return Err(format!(
                "voiceSamples: {} hold {positives} positive takes, {} of them numbered a multiple of \
                 {HOLD_OUT_EVERY}; a training needs at least one to hold out and one to train \
                 (record {MIN_POSITIVES} or more)",
                sets.join(", "),
                own.held_out.len()
            ));
        }
        Ok(own)
    }

    /// Paths for the trainer's `request.json`.
    pub fn request(&self) -> serde_json::Value {
        let paths = |takes: &[TakeFile]| {
            takes
                .iter()
                .map(|t| t.path.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        serde_json::json!({
            "trainPositive": paths(&self.train_positive),
            "heldOut": paths(&self.held_out),
            "nearMiss": paths(&self.near_miss),
            "speech": paths(&self.speech),
        })
    }
}

/// Recall on the held-out takes at the chosen cutoff.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnVoiceRecall {
    pub recall: f64,
    pub accepted: usize,
    pub held_out: usize,
}

impl OwnVoiceRecall {
    pub fn at(scores: &[f64], cutoff: f64) -> Self {
        let accepted = scores.iter().filter(|s| **s > cutoff).count();
        OwnVoiceRecall {
            recall: if scores.is_empty() {
                0.0
            } else {
                accepted as f64 / scores.len() as f64
            },
            accepted,
            held_out: scores.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn take(kind: Kind, number: u32) -> TakeFile {
        TakeFile {
            set: "s1".into(),
            kind,
            number,
            phrase: (kind == Kind::NearMiss).then(|| "hey_luna".to_string()),
            path: PathBuf::from(format!("/v/s1/{}/{number:03}.wav", kind.as_str())),
            sha256: "ab".into(),
        }
    }

    #[test]
    fn every_fourth_positive_is_held_out_and_the_rest_train() {
        let mut takes: Vec<TakeFile> = (1..=60).map(|n| take(Kind::Positive, n)).collect();
        takes.push(take(Kind::NearMiss, 1));
        takes.push(take(Kind::Speech, 1));
        let own = OwnVoice::split(&["s1".into()], &takes).unwrap();
        assert_eq!(own.held_out.len(), 15);
        assert_eq!(own.train_positive.len(), 45);
        assert!(own.held_out.iter().all(|t| t.number % 4 == 0));
        assert_eq!((own.near_miss.len(), own.speech.len()), (1, 1));
        let request = own.request();
        assert_eq!(request["heldOut"][0], "/v/s1/positive/004.wav");
        assert_eq!(request["speech"][0], "/v/s1/speech/001.wav");
    }

    #[test]
    fn a_set_with_nothing_to_hold_out_is_refused() {
        let three: Vec<TakeFile> = (1..=3).map(|n| take(Kind::Positive, n)).collect();
        let error = OwnVoice::split(&["s1".into()], &three).unwrap_err();
        assert!(error.contains("3 positive takes, 0 of them"), "{error}");
        assert!(OwnVoice::split(&["s1".into()], &[take(Kind::Positive, 4)]).is_err());
    }

    #[test]
    fn own_voice_recall_counts_scores_above_the_cutoff() {
        assert_eq!(
            OwnVoiceRecall::at(&[0.9, 0.95, 0.5, 0.2], 0.89),
            OwnVoiceRecall {
                recall: 0.5,
                accepted: 2,
                held_out: 4
            }
        );
        assert_eq!(OwnVoiceRecall::at(&[], 0.5).recall, 0.0);
    }
}

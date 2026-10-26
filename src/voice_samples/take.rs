//! What a take is called, and whether its audio is a take (`docs/engines/wake-word.md`, "Names").
//! Pure: names and bytes in, a take or a named refusal out.

use serde::Serialize;

/// The longest take: the recording guide's longest is 30 s, plus its timer's slack.  Safe range 30 to 60 s.
pub const MAX_TAKE_SECONDS: f64 = 31.0;
/// The shortest take: shorter is not a word.  Safe range 0.1 to 1 s.
pub const MIN_TAKE_SECONDS: f64 = 0.25;
/// The largest upload: 31 s of 16 kHz PCM16 is 992 044 bytes with its header.  Safe range 1 to 4 MiB.
pub const MAX_TAKE_BYTES: u64 = 1024 * 1024;
const SAMPLE_RATE: u32 = 16_000;
const MAX_SET_CHARS: usize = 64;
const MAX_NUMBER_DIGITS: usize = 4;

/// What a take holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The phrase.
    Positive,
    /// A near miss, named in the take.
    NearMiss,
    /// Talk without the phrase.
    Speech,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Positive, Kind::NearMiss, Kind::Speech];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Positive => "positive",
            Kind::NearMiss => "near_miss",
            Kind::Speech => "speech",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == text)
    }
}

/// One take's identity: its kind, number and (for a near miss) the near miss's slug.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Take {
    pub kind: Kind,
    pub number: u32,
    pub phrase: Option<String>,
}

impl Take {
    /// The file stem it is stored as: `007`, `hey_luna-002`.
    pub fn stem(&self) -> String {
        match &self.phrase {
            Some(phrase) => format!("{phrase}-{:03}", self.number),
            None => format!("{:03}", self.number),
        }
    }
}

/// Why a name is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadName(pub String);

fn digits(text: &str) -> Option<u32> {
    let ok = !text.is_empty()
        && text.len() <= MAX_NUMBER_DIGITS
        && text.bytes().all(|b| b.is_ascii_digit());
    ok.then(|| text.parse().ok()).flatten()
}

/// A near miss's slug: `[a-z0-9]+(_[a-z0-9]+)*`.
pub fn is_slug(text: &str) -> bool {
    !text.is_empty()
        && text.split('_').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// A set's name: `[a-z0-9][a-z0-9_-]{0,63}`.
pub fn check_set(set: &str) -> Result<(), BadName> {
    let mut bytes = set.bytes();
    let first_ok = bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    let rest_ok =
        bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if !(first_ok && rest_ok && set.len() <= MAX_SET_CHARS) {
        return Err(BadName(format!(
            "set {set:?}: 1 to {MAX_SET_CHARS} of a-z, 0-9, _ and -, starting with a letter or digit"
        )));
    }
    Ok(())
}

/// A take of `kind` named `name` (without `.wav`).
pub fn parse_take(kind: Kind, name: &str) -> Result<Take, BadName> {
    let refuse = |rule: &str| {
        Err(BadName(format!(
            "take {name:?} of {}: {rule}",
            kind.as_str()
        )))
    };
    match kind {
        Kind::Positive | Kind::Speech => match digits(name) {
            Some(number) => Ok(Take {
                kind,
                number,
                phrase: None,
            }),
            None => refuse("its number, 1 to 4 digits"),
        },
        Kind::NearMiss => {
            let parsed = name
                .rsplit_once('-')
                .and_then(|(phrase, number)| Some((phrase, digits(number)?)))
                .filter(|(phrase, _)| is_slug(phrase));
            match parsed {
                Some((phrase, number)) => Ok(Take {
                    kind,
                    number,
                    phrase: Some(phrase.to_string()),
                }),
                None => refuse("the near miss's slug and its number, e.g. hey_luna-002"),
            }
        }
    }
}

/// `/voice-samples/<set>/<kind>/<take>.wav` read into its set and take.
pub fn parse_path(path: &str) -> Result<(String, Take), BadName> {
    let parts: Vec<&str> = path
        .strip_prefix("/voice-samples/")
        .map(|rest| rest.split('/').collect())
        .unwrap_or_default();
    let [set, kind, file] = parts.as_slice() else {
        return Err(BadName(format!(
            "{path:?} is not /voice-samples/<set>/<kind>/<take>.wav"
        )));
    };
    check_set(set)?;
    let Some(kind) = Kind::parse(kind) else {
        return Err(BadName(format!(
            "kind {kind:?}: one of positive, near_miss, speech"
        )));
    };
    let Some(name) = file.strip_suffix(".wav") else {
        return Err(BadName(format!("take {file:?}: a .wav file")));
    };
    Ok((set.to_string(), parse_take(kind, name)?))
}

/// What a valid take's audio is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Audio {
    pub seconds: f64,
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Whether `bytes` are a 16 kHz mono PCM16 WAV of `MIN_TAKE_SECONDS` to `MAX_TAKE_SECONDS`.
pub fn check_wav(bytes: &[u8]) -> Result<Audio, String> {
    if bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return Err("not a WAV file (no RIFF/WAVE header)".into());
    }
    let mut at = 12;
    let mut format = None;
    let mut data = None;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32_at(bytes, at + 4).expect("8 bytes are there") as usize;
        let body = at + 8;
        match id {
            b"fmt " => {
                format = Some((
                    u16_at(bytes, body),
                    u16_at(bytes, body + 2),
                    u32_at(bytes, body + 4),
                    u16_at(bytes, body + 14),
                ))
            }
            b"data" => {
                data = Some(size.min(bytes.len().saturating_sub(body)));
                if body + size > bytes.len() {
                    return Err(format!(
                        "its data chunk says {size} bytes, the file holds {}",
                        bytes.len() - body
                    ));
                }
            }
            _ => {}
        }
        // Chunks are padded to an even size.
        at = body.saturating_add(size).saturating_add(size % 2);
    }
    let Some((encoding, channels, rate, bits)) = format else {
        return Err("no fmt chunk".into());
    };
    if encoding != Some(1) || bits != Some(16) {
        return Err(format!(
            "not 16-bit PCM (format {encoding:?}, {bits:?} bits)"
        ));
    }
    if channels != Some(1) {
        return Err(format!("not mono ({channels:?} channels)"));
    }
    if rate != Some(SAMPLE_RATE) {
        return Err(format!("not 16 kHz ({rate:?} Hz)"));
    }
    let Some(data) = data else {
        return Err("no data chunk".into());
    };
    let seconds = data as f64 / (f64::from(SAMPLE_RATE) * 2.0);
    if seconds < MIN_TAKE_SECONDS {
        return Err(format!(
            "{seconds:.2} s is shorter than {MIN_TAKE_SECONDS} s"
        ));
    }
    if seconds > MAX_TAKE_SECONDS {
        return Err(format!(
            "{seconds:.1} s is longer than {MAX_TAKE_SECONDS} s"
        ));
    }
    Ok(Audio { seconds })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A WAV of `seconds` of silence in the given format.
    pub(crate) fn wav_with(
        seconds: f64,
        rate: u32,
        channels: u16,
        bits: u16,
        encoding: u16,
    ) -> Vec<u8> {
        let frames = (seconds * f64::from(rate)) as u32;
        let data = frames * u32::from(channels) * u32::from(bits / 8);
        let mut out = Vec::new();
        out.extend(b"RIFF");
        out.extend((36 + data).to_le_bytes());
        out.extend(b"WAVEfmt ");
        out.extend(16u32.to_le_bytes());
        out.extend(encoding.to_le_bytes());
        out.extend(channels.to_le_bytes());
        out.extend(rate.to_le_bytes());
        out.extend((rate * u32::from(channels) * u32::from(bits / 8)).to_le_bytes());
        out.extend((channels * bits / 8).to_le_bytes());
        out.extend(bits.to_le_bytes());
        out.extend(b"data");
        out.extend(data.to_le_bytes());
        out.extend(std::iter::repeat_n(0u8, data as usize));
        out
    }

    pub(crate) fn wav(seconds: f64) -> Vec<u8> {
        wav_with(seconds, 16_000, 1, 16, 1)
    }

    #[test]
    fn a_take_of_the_guide_passes() {
        assert_eq!(check_wav(&wav(2.0)), Ok(Audio { seconds: 2.0 }));
        assert_eq!(check_wav(&wav(30.0)).unwrap().seconds, 30.0);
    }

    #[test]
    fn every_other_audio_is_refused_by_what_is_wrong() {
        let refused = |bytes: Vec<u8>| check_wav(&bytes).unwrap_err();
        assert!(refused(b"hello".to_vec()).contains("RIFF"));
        assert!(refused(wav_with(2.0, 44_100, 1, 16, 1)).contains("16 kHz"));
        assert!(refused(wav_with(2.0, 16_000, 2, 16, 1)).contains("mono"));
        assert!(refused(wav_with(2.0, 16_000, 1, 32, 3)).contains("16-bit PCM"));
        assert!(refused(wav(0.1)).contains("shorter"));
        assert!(refused(wav(31.5)).contains("longer"));
        let mut truncated = wav(2.0);
        truncated.truncate(1000);
        assert!(refused(truncated).contains("data chunk says"));
        let mut no_data = wav(2.0);
        no_data.truncate(36);
        assert!(refused(no_data).contains("no data chunk"));
        assert!(refused(b"RIFF\0\0\0\0WAVE".to_vec()).contains("no fmt chunk"));
    }

    #[test]
    fn chunks_before_the_audio_are_skipped() {
        let plain = wav(1.0);
        let mut with_list = plain[..12].to_vec();
        with_list.extend(b"LIST");
        with_list.extend(3u32.to_le_bytes());
        with_list.extend([1, 2, 3, 0]); // odd size, padded
        with_list.extend(&plain[12..]);
        assert_eq!(check_wav(&with_list).unwrap().seconds, 1.0);
    }

    #[test]
    fn paths_read_into_set_and_take() {
        let (set, take) = parse_path("/voice-samples/2026-10-26/positive/007.wav").unwrap();
        assert_eq!(set, "2026-10-26");
        assert_eq!(
            take,
            Take {
                kind: Kind::Positive,
                number: 7,
                phrase: None
            }
        );
        assert_eq!(take.stem(), "007");
        let (_, take) = parse_path("/voice-samples/s1/near_miss/hey_luna-002.wav").unwrap();
        assert_eq!(take.phrase.as_deref(), Some("hey_luna"));
        assert_eq!((take.number, take.stem()), (2, "hey_luna-002".to_string()));
        assert_eq!(
            parse_path("/voice-samples/s1/speech/3.wav")
                .unwrap()
                .1
                .stem(),
            "003"
        );
    }

    #[test]
    fn unsafe_and_unknown_names_are_refused() {
        for path in [
            "/voice-samples/../positive/001.wav",
            "/voice-samples/.hidden/positive/001.wav",
            "/voice-samples/Set/positive/001.wav",
            "/voice-samples/s1/positive/../../x.wav",
            "/voice-samples/s1/laugh/001.wav",
            "/voice-samples/s1/positive/001.mp3",
            "/voice-samples/s1/positive/12345.wav",
            "/voice-samples/s1/positive/abc.wav",
            "/voice-samples/s1/near_miss/001.wav",
            "/voice-samples/s1/near_miss/Hey_Luna-001.wav",
            "/voice-samples/s1/near_miss/hey__luna-001.wav",
            "/voice-samples/s1/positive",
            "/voice-samples/s1/positive/001.wav/x",
            "/elsewhere/s1/positive/001.wav",
        ] {
            assert!(parse_path(path).is_err(), "{path} is refused");
        }
        assert!(check_set(&"a".repeat(65)).is_err());
        assert!(check_set(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn kinds_spell_as_the_routes_do() {
        for kind in Kind::ALL {
            assert_eq!(Kind::parse(kind.as_str()), Some(kind));
        }
        assert!(is_slug("hey_luna") && is_slug("okay_nabu2") && !is_slug("_x") && !is_slug(""));
    }
}

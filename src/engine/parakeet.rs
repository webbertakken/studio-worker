//! Streaming speech-to-text over parakeet-rs, kept loaded by the model
//! host.  Two model families: Nemotron streaming (560 ms chunks,
//! multilingual, punctuated) and Parakeet EOU (160 ms chunks, English,
//! end-of-utterance aware).  The weights load once; every session opens
//! a fresh utterance state over them.

use crate::catalog::CatalogModel;
use crate::engine::onnx_provision::{self, OrtFlavour};
use crate::host::{LoadedModel, StreamingModel};
use crate::stt_stream::session::StreamingTranscriber;
use anyhow::{anyhow, Context, Result};
use parakeet_rs::{ExecutionConfig, Nemotron, NemotronHandle, ParakeetEOU, ParakeetEOUHandle};
use std::path::{Path, PathBuf};
use std::time::Instant;

const TRACE_TARGET: &str = "studio_worker::engine::parakeet";

/// Samples per Nemotron streaming step (560 ms at 16 kHz).
pub const NEMOTRON_CHUNK: usize = 8960;
/// Samples per Parakeet EOU step (160 ms at 16 kHz).
pub const EOU_CHUNK: usize = 2560;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Nemotron,
    Eou,
}

/// Which family a model's files are: EOU ships `tokenizer.json`, Nemotron
/// a SentencePiece `tokenizer.model`.
pub fn stream_kind<'a>(filenames: impl IntoIterator<Item = &'a str>) -> Option<StreamKind> {
    let names: Vec<&str> = filenames.into_iter().collect();
    if names.contains(&"tokenizer.model") {
        Some(StreamKind::Nemotron)
    } else if names.contains(&"tokenizer.json") {
        Some(StreamKind::Eou)
    } else {
        None
    }
}

/// Where a streaming model's files live: one directory per model, as the
/// loaders read the whole directory.
pub fn model_dir(models_root: &Path, model_id: &str) -> PathBuf {
    models_root.join("stt").join(model_id)
}

enum Handle {
    Nemotron(NemotronHandle),
    Eou(ParakeetEOUHandle),
}

/// A streaming speech model held in memory by the model host.
pub struct LoadedStream {
    handle: Handle,
}

impl LoadedModel for LoadedStream {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_stream(&self) -> Option<&dyn StreamingModel> {
        Some(self)
    }
}

impl StreamingModel for LoadedStream {
    fn open(&self) -> Result<Box<dyn StreamingTranscriber + '_>> {
        Ok(match &self.handle {
            Handle::Nemotron(h) => Box::new(NemotronStream(Nemotron::from_shared(h))),
            Handle::Eou(h) => Box::new(EouStream(ParakeetEOU::from_shared(h))),
        })
    }
}

struct NemotronStream(Nemotron);

impl StreamingTranscriber for NemotronStream {
    fn chunk_samples(&self) -> usize {
        NEMOTRON_CHUNK
    }
    fn step(&mut self, chunk: &[f32]) -> Result<String> {
        Ok(self.0.transcribe_chunk(chunk)?)
    }
    fn reset(&mut self) {
        self.0.reset();
    }
}

struct EouStream(ParakeetEOU);

impl StreamingTranscriber for EouStream {
    fn chunk_samples(&self) -> usize {
        EOU_CHUNK
    }
    fn step(&mut self, chunk: &[f32]) -> Result<String> {
        Ok(self.0.transcribe(chunk, false)?)
    }
    /// Every session opens a fresh EOU state (`from_shared`), so there is
    /// nothing to reset.
    fn reset(&mut self) {}
}

/// Session options for the process's ONNX Runtime flavour.  On CUDA the
/// provider is registered strictly: if it cannot load, the model fails
/// to load rather than running on the CPU unnoticed.
fn exec_config(flavour: OrtFlavour) -> ExecutionConfig {
    let config = ExecutionConfig::new();
    if flavour.is_cuda() {
        config.with_custom_configure(|builder| {
            Ok(builder
                .with_execution_providers([ort::ep::CUDA::default().build().error_on_failure()])?)
        })
    } else {
        config
    }
}

/// Download (if needed) and load `model` for the host to keep resident.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn load_resident(models_root: &Path, model: &CatalogModel) -> Result<LoadedStream> {
    let runtime = onnx_provision::ensure_runtime(models_root)?;
    let dir = model_dir(models_root, &model.id);
    for file in &model.source.files {
        crate::engine::download::ensure_file(&dir, file)
            .with_context(|| format!("downloading {} for {}", file.filename, model.id))?;
    }
    let kind =
        stream_kind(model.source.files.iter().map(|f| f.filename.as_str())).ok_or_else(|| {
            anyhow!(
            "{}: a streaming speech model needs tokenizer.model (Nemotron) or tokenizer.json (EOU)",
            model.id
        )
        })?;
    if !runtime.flavour.is_cuda() {
        tracing::warn!(
            target: TRACE_TARGET,
            op = "load",
            model = %model.id,
            flavour = runtime.flavour.name(),
            "no CUDA runtime on this host; the speech model runs on the CPU"
        );
    }
    let started = Instant::now();
    let config = Some(exec_config(runtime.flavour));
    let handle = match kind {
        StreamKind::Nemotron => Handle::Nemotron(NemotronHandle::from_pretrained(&dir, config)?),
        StreamKind::Eou => Handle::Eou(ParakeetEOUHandle::from_pretrained(&dir, config)?),
    };
    tracing::info!(
        target: TRACE_TARGET,
        op = "load",
        model = %model.id,
        kind = ?kind,
        flavour = runtime.flavour.name(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "streaming speech model loaded"
    );
    Ok(LoadedStream { handle })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tokenizer_file_names_the_family() {
        assert_eq!(
            stream_kind(["encoder.onnx", "decoder_joint.onnx", "tokenizer.model"]),
            Some(StreamKind::Nemotron)
        );
        assert_eq!(
            stream_kind(["encoder.onnx", "decoder_joint.onnx", "tokenizer.json"]),
            Some(StreamKind::Eou)
        );
        assert_eq!(stream_kind(["encoder.onnx"]), None);
    }

    #[test]
    fn each_model_gets_its_own_directory() {
        assert_eq!(
            model_dir(Path::new("/m"), "nemotron-3.5"),
            Path::new("/m/stt/nemotron-3.5")
        );
    }
}

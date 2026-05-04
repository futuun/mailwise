use anyhow::Result;
use llama_cpp_2::context::params::{LlamaContextParams, LlamaPoolingType};
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use std::num::NonZeroU32;

mod model;

/// Full training context for jina-embeddings-v5-text-nano-retrieval.
const MAX_SEQ_TOKENS: usize = 8192;

/// llama.cpp wrapper running on Metal.
pub struct Embedder {
    backend: LlamaBackend,
    model: LlamaModel,
}

impl Embedder {
    /// Downloads the model on first call.
    pub fn new() -> Result<Self> {
        let model_path = model::ensure_model()?;

        let backend = LlamaBackend::init()
            .map_err(|e| anyhow::anyhow!("Failed to initialize llama backend: {}", e))?;

        llama_cpp_2::send_logs_to_tracing(
            llama_cpp_2::LogOptions::default().with_logs_enabled(false),
        );

        // 1000 == "all GPU layers"; llama.cpp caps internally based on
        // the model size and available VRAM.
        let model_params = LlamaModelParams::default().with_n_gpu_layers(1000);

        let model =
            LlamaModel::load_from_file(&backend, &model_path, &model_params).map_err(|e| {
                anyhow::anyhow!(
                    "Failed to load GGUF model from {}: {}",
                    model_path.display(),
                    e
                )
            })?;

        Ok(Self { backend, model })
    }

    /// One vector per input. Caller should pre-sort by length: the
    /// context allocated by [`Embedder::embed_tokenized`] is sized to
    /// the batch's longest sequence, so mixed-length batches waste
    /// KV-cache memory on the shorter ones.
    pub fn embed_documents(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let tokenized: Vec<_> = texts
            .iter()
            .map(|t| self.tokenize(&format!("Document:{}", t)))
            .collect::<Result<Vec<_>>>()?;

        self.embed_tokenized(&tokenized)
    }

    pub fn embed_query(&mut self, text: &str) -> Result<Vec<f32>> {
        let tokens = self.tokenize(&format!("Query:{}", text))?;
        let results = self.embed_tokenized(&[tokens])?;
        Ok(results.into_iter().next().unwrap())
    }

    fn tokenize(&self, text: &str) -> Result<Vec<llama_cpp_2::token::LlamaToken>> {
        let mut tokens = self
            .model
            .str_to_token(text, AddBos::Never)
            .map_err(|e| anyhow::anyhow!("Tokenization failed: {}", e))?;
        tokens.truncate(MAX_SEQ_TOKENS - 2);

        Ok([
            &[self.model.token_bos()],
            &tokens[..],
            &[self.model.token_eos()],
        ]
        .concat())
    }

    /// Sequential decode sharing one context sized to the longest
    /// sequence in this batch.
    ///
    /// Fresh `LlamaContext` per batch is intentional. `n_ctx`/`n_batch`
    /// scale with the longest sequence; reusing a long-lived context
    /// would over-allocate KV-cache memory for the many small batches
    /// most of an index run consists of. Length-sorted batching at the
    /// caller keeps each context tight.
    fn embed_tokenized(
        &mut self,
        tokenized: &[Vec<llama_cpp_2::token::LlamaToken>],
    ) -> Result<Vec<Vec<f32>>> {
        let n_embd = self.model.n_embd() as usize;
        let max_len = tokenized.iter().map(|t| t.len()).max().unwrap_or(1);

        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(Some(NonZeroU32::new(max_len as u32).unwrap()))
            .with_n_batch(max_len as u32)
            .with_n_ubatch(max_len as u32)
            .with_embeddings(true)
            .with_pooling_type(LlamaPoolingType::Last);

        let mut ctx = self
            .model
            .new_context(&self.backend, ctx_params)
            .map_err(|e| anyhow::anyhow!("Failed to create context: {}", e))?;

        let mut results = Vec::with_capacity(tokenized.len());

        for tokens in tokenized {
            let mut batch = LlamaBatch::new(tokens.len(), 1);
            batch
                .add_sequence(tokens, 0, false)
                .map_err(|e| anyhow::anyhow!("Failed to add sequence to batch: {}", e))?;

            ctx.clear_kv_cache();
            ctx.decode(&mut batch)
                .map_err(|e| anyhow::anyhow!("Decode failed: {}", e))?;

            let embedding = ctx
                .embeddings_seq_ith(0)
                .map_err(|e| anyhow::anyhow!("Failed to get embeddings: {}", e))?;

            // L2-normalize so cosine similarity == dot product in sqlite-vec KNN.
            // Without this, longer texts would have larger magnitudes, biasing search.
            let mut vec = embedding[..n_embd].to_vec();
            let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for val in &mut vec {
                    *val /= norm;
                }
            }

            results.push(vec);
        }

        Ok(results)
    }
}

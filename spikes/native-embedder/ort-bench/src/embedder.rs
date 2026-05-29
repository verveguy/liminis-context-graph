use anyhow::{Context, Result};
#[cfg(target_os = "macos")]
use ort::execution_providers::CoreMLExecutionProvider;
use ort::{session::Session, value::TensorRef};
use tokenizers::Tokenizer;

const MAX_SEQ_LEN: usize = 512;

pub struct BgeEmbedder {
    session: Session,
    tokenizer: Tokenizer,
}

impl BgeEmbedder {
    pub fn load(model_dir: &std::path::Path) -> Result<Self> {
        let onnx_path = model_dir.join("model.onnx");
        let session = Session::builder()
            .context("creating ort Session builder")?
            .commit_from_file(&onnx_path)
            .with_context(|| format!("loading ONNX model from {}", onnx_path.display()))?;

        let tokenizer_path = model_dir.join("tokenizer.json");
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("{e}"))
            .with_context(|| format!("loading {}", tokenizer_path.display()))?;

        Ok(BgeEmbedder { session, tokenizer })
    }

    #[cfg(target_os = "macos")]
    pub fn load_with_coreml(model_dir: &std::path::Path) -> Result<Self> {
        let onnx_path = model_dir.join("model.onnx");
        let session = Session::builder()
            .context("creating ort Session builder")?
            .with_execution_providers([CoreMLExecutionProvider::default().build()])
            .map_err(|e| anyhow::anyhow!("registering CoreML execution provider: {e}"))?
            .commit_from_file(&onnx_path)
            .with_context(|| format!("loading ONNX model from {}", onnx_path.display()))?;

        let tokenizer_path = model_dir.join("tokenizer.json");
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("{e}"))
            .with_context(|| format!("loading {}", tokenizer_path.display()))?;

        Ok(BgeEmbedder { session, tokenizer })
    }

    pub fn embed(&mut self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        // Truncate to model's max length
        let ids = encoding.get_ids();
        let mask = encoding.get_attention_mask();
        let type_ids = encoding.get_type_ids();
        let seq_len = ids.len().min(MAX_SEQ_LEN);

        let input_ids: Vec<i64> = ids[..seq_len].iter().map(|&x| x as i64).collect();
        let attention_mask: Vec<i64> = mask[..seq_len].iter().map(|&x| x as i64).collect();
        let token_type_ids: Vec<i64> = type_ids[..seq_len].iter().map(|&x| x as i64).collect();
        let shape = [1i64, seq_len as i64];

        let outputs = self.session.run(ort::inputs![
            "input_ids" => TensorRef::from_array_view((&shape[..], input_ids.as_slice()))?,
            "attention_mask" => TensorRef::from_array_view((&shape[..], attention_mask.as_slice()))?,
            "token_type_ids" => TensorRef::from_array_view((&shape[..], token_type_ids.as_slice()))?,
        ])?;

        // last_hidden_state shape: [1, seq_len, 768]
        // CLS token is at flat indices [0..hidden_size]
        let (shape, data) = outputs[0].try_extract_tensor::<f32>()?;
        let hidden_size = shape[2] as usize;
        let cls_vec: Vec<f32> = data[..hidden_size].to_vec();

        // L2 normalize
        let norm: f32 = cls_vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        Ok(cls_vec.iter().map(|x| x / (norm + 1e-10)).collect())
    }

    #[allow(dead_code)]
    pub fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed(t)?);
        }
        Ok(out)
    }
}

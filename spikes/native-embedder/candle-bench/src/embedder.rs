use anyhow::{Context, Result};
use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use tokenizers::Tokenizer;

pub struct BgeEmbedder {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl BgeEmbedder {
    pub fn load(model_dir: &std::path::Path) -> Result<Self> {
        let device = Device::Cpu;

        let config_path = model_dir.join("config.json");
        let config: Config = serde_json::from_reader(
            std::fs::File::open(&config_path)
                .with_context(|| format!("opening {}", config_path.display()))?,
        )
        .context("parsing config.json")?;

        let model_path = model_dir.join("model.safetensors");
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[&model_path], DType::F32, &device)
                .with_context(|| format!("loading {}", model_path.display()))?
        };

        let model = BertModel::load(vb, &config).context("loading BertModel")?;

        let tokenizer_path = model_dir.join("tokenizer.json");
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("{e}"))
            .with_context(|| format!("loading {}", tokenizer_path.display()))?;

        Ok(BgeEmbedder {
            model,
            tokenizer,
            device,
        })
    }

    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let ids: Vec<u32> = encoding.get_ids().to_vec();
        let type_ids: Vec<u32> = encoding.get_type_ids().to_vec();
        let mask: Vec<u32> = encoding.get_attention_mask().to_vec();

        let input_ids = Tensor::new(ids.as_slice(), &self.device)?.unsqueeze(0)?;
        let token_type_ids = Tensor::new(type_ids.as_slice(), &self.device)?.unsqueeze(0)?;
        let attention_mask = Tensor::new(mask.as_slice(), &self.device)?.unsqueeze(0)?;

        // forward: (batch=1, seq, hidden=768)
        let output = self
            .model
            .forward(&input_ids, &token_type_ids, Some(&attention_mask))?;

        // CLS pooling: take token at position 0
        let cls = output.i((0, 0))?; // (hidden,)
        let cls_vec: Vec<f32> = cls.to_vec1()?;

        // L2 normalize
        let norm: f32 = cls_vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        Ok(cls_vec.iter().map(|x| x / (norm + 1e-10)).collect())
    }

    #[allow(dead_code)]
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
}

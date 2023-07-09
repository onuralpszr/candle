use crate::nn::{Embedding, HiddenAct, LayerNorm, Linear, VarBuilder};
use crate::{encodec_model, t5_model};
use anyhow::Result;
use candle::{DType, Device, Tensor, D};

// https://github.com/huggingface/transformers/blob/cd4584e3c809bb9e1392ccd3fe38b40daba5519a/src/transformers/models/musicgen/configuration_musicgen.py#L83
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    vocab_size: usize,
    max_position_embeddings: usize,
    num_hidden_layers: usize,
    ffn_dim: usize,
    num_attention_heads: usize,
    layerdrop: f64,
    use_cache: bool,
    activation_function: HiddenAct,
    hidden_size: usize,
    dropout: f64,
    attention_dropout: f64,
    activation_dropout: f64,
    initializer_factor: f64,
    scale_embedding: bool,
    num_codebooks: usize,
    pad_token_id: usize,
    bos_token_id: usize,
    eos_token_id: Option<usize>,
    tie_word_embeddings: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            vocab_size: 2048,
            max_position_embeddings: 2048,
            num_hidden_layers: 24,
            ffn_dim: 4096,
            num_attention_heads: 16,
            layerdrop: 0.0,
            use_cache: true,
            activation_function: HiddenAct::Gelu, // TODO: Handle old style gelu.
            hidden_size: 1024,
            dropout: 0.1,
            attention_dropout: 0.0,
            activation_dropout: 0.0,
            initializer_factor: 0.02,
            scale_embedding: false,
            num_codebooks: 4,
            pad_token_id: 2048,
            bos_token_id: 2048,
            eos_token_id: None,
            tie_word_embeddings: false,
        }
    }
}

impl Config {
    fn musicgen_small() -> Self {
        Self {
            vocab_size: 2048,
            max_position_embeddings: 2048,
            num_hidden_layers: 24,
            ffn_dim: 4096,
            num_attention_heads: 16,
            layerdrop: 0.0,
            use_cache: true,
            activation_function: HiddenAct::Gelu, // TODO: Handle old style gelu.
            hidden_size: 1024,
            dropout: 0.1,
            attention_dropout: 0.0,
            activation_dropout: 0.0,
            initializer_factor: 0.02,
            scale_embedding: false,
            num_codebooks: 4,
            pad_token_id: 2048,
            bos_token_id: 2048,
            eos_token_id: None,
            tie_word_embeddings: false,
        }
    }
}

fn get_embedding(num_embeddings: usize, embedding_dim: usize) -> Result<Tensor> {
    let half_dim = embedding_dim / 2;
    let emb = f64::ln(10000.) / (half_dim - 1) as f64;
    let xs: Vec<_> = (0..num_embeddings).map(|v| v as f32).collect();
    let xs = Tensor::from_vec(xs, (num_embeddings, 1), &Device::Cpu)?;
    let ys: Vec<_> = (0..half_dim)
        .map(|v| f64::exp(v as f64 * -emb) as f32)
        .collect();
    let ys = Tensor::from_vec(ys, (1, half_dim), &Device::Cpu)?;
    let shape = (num_embeddings, half_dim);
    let emb = (xs.broadcast_as(shape)? * ys.broadcast_as(shape)?)?;
    let emb =
        Tensor::cat(&[&emb.cos()?, &emb.sin()?], 1)?.reshape((num_embeddings, 2 * half_dim))?;
    let emb = if embedding_dim % 2 == 1 {
        let zeros = Tensor::zeros((num_embeddings, 1), DType::F32, &Device::Cpu)?;
        Tensor::cat(&[&emb, &zeros], 1)?
    } else {
        emb
    };
    Ok(emb)
}

#[derive(Debug)]
struct MusicgenSinusoidalPositionalEmbedding {
    num_positions: usize,
    embedding_dim: usize,
    weights: Tensor,
}

impl MusicgenSinusoidalPositionalEmbedding {
    fn load(_vb: &VarBuilder, cfg: &Config) -> Result<Self> {
        let num_positions = cfg.max_position_embeddings;
        let embedding_dim = cfg.hidden_size;
        let weights = get_embedding(num_positions, embedding_dim)?;
        Ok(Self {
            num_positions,
            embedding_dim,
            weights,
        })
    }

    fn forward(&mut self, input_ids: &Tensor) -> Result<Tensor> {
        let (_b_sz, _codebooks, seq_len) = input_ids.shape().r3()?;
        if seq_len > self.weights.dim(0)? {
            self.weights = get_embedding(seq_len, self.embedding_dim)?
        }
        Ok(self.weights.narrow(0, 0, seq_len)?)
    }
}

#[derive(Debug)]
struct MusicgenAttention {
    scaling: f64,
    is_decoder: bool,
    num_heads: usize,
    head_dim: usize,
    k_proj: Linear,
    v_proj: Linear,
    q_proj: Linear,
    out_proj: Linear,
}

impl MusicgenAttention {
    fn load(p: &str, vb: &VarBuilder, cfg: &Config) -> Result<Self> {
        let h = cfg.hidden_size;
        let num_heads = cfg.num_attention_heads;
        let head_dim = h / num_heads;
        let k_proj = Linear::load(h, h, false, &format!("{p}.k_proj"), vb)?;
        let v_proj = Linear::load(h, h, false, &format!("{p}.v_proj"), vb)?;
        let q_proj = Linear::load(h, h, false, &format!("{p}.q_proj"), vb)?;
        let out_proj = Linear::load(h, h, false, &format!("{p}.out_proj"), vb)?;
        Ok(Self {
            scaling: 1. / (head_dim as f64).sqrt(),
            is_decoder: true,
            num_heads,
            head_dim,
            k_proj,
            v_proj,
            q_proj,
            out_proj,
        })
    }

    fn forward(
        &mut self,
        xs: &Tensor,
        kv_states: Option<&Tensor>,
        attention_mask: &Tensor,
    ) -> Result<Tensor> {
        let (b_sz, tgt_len, _) = xs.shape().r3()?;
        let query_states = (self.q_proj.forward(xs)? * self.scaling)?;

        let kv_states = kv_states.unwrap_or(xs);
        let key_states = self.k_proj.forward(kv_states)?;
        let value_states = self.v_proj.forward(kv_states)?;

        let tgt = (b_sz, tgt_len, self.num_heads, self.head_dim);
        let query_states = query_states.reshape(tgt)?.transpose(1, 2)?.contiguous()?;
        let key_states = key_states.reshape(tgt)?.transpose(1, 2)?.contiguous()?;
        let value_states = value_states.reshape(tgt)?.transpose(1, 2)?.contiguous()?;

        let src_len = key_states.dim(1)?;
        let attn_weights = query_states.matmul(&key_states.transpose(1, 2)?)?;
        let attn_weights = attn_weights
            .reshape((b_sz, self.num_heads, tgt_len, src_len))?
            .broadcast_add(attention_mask)?;
        let attn_weights = attn_weights.softmax(D::Minus1)?;
        // TODO: layer_head_mask?
        let attn_output = attn_weights
            .matmul(&value_states)?
            .reshape((b_sz, self.num_heads, tgt_len, self.head_dim))?
            .transpose(1, 2)?
            .reshape((b_sz, tgt_len, self.num_heads * self.head_dim))?;
        let attn_output = self.out_proj.forward(&attn_output)?;
        Ok(attn_output)
    }
}

#[derive(Debug)]
struct MusicgenDecoderLayer {
    self_attn: MusicgenAttention,
    self_attn_layer_norm: LayerNorm,
    encoder_attn: MusicgenAttention,
    encoder_attn_layer_norm: LayerNorm,
    fc1: Linear,
    fc2: Linear,
    final_layer_norm: LayerNorm,
    activation_fn: HiddenAct,
}

impl MusicgenDecoderLayer {
    fn load(p: &str, vb: &VarBuilder, cfg: &Config) -> Result<Self> {
        let h = cfg.hidden_size;
        let self_attn = MusicgenAttention::load(&format!("{p}.self_attn"), vb, cfg)?;
        let self_attn_layer_norm =
            LayerNorm::load(h, 1e-5, &format!("{p}.self_attn_layer_norm"), vb)?;
        let encoder_attn = MusicgenAttention::load(&format!("{p}.encoder_attn"), vb, cfg)?;
        let encoder_attn_layer_norm =
            LayerNorm::load(h, 1e-5, &format!("{p}.encoder_attn_layer_norm"), vb)?;
        let fc1 = Linear::load(h, cfg.ffn_dim, false, &format!("{p}.fc1"), vb)?;
        let fc2 = Linear::load(cfg.ffn_dim, h, false, &format!("{p}.fc2"), vb)?;
        let final_layer_norm = LayerNorm::load(h, 1e-5, &format!("{p}.final_layer_norm"), vb)?;
        Ok(Self {
            self_attn,
            self_attn_layer_norm,
            encoder_attn,
            encoder_attn_layer_norm,
            fc1,
            fc2,
            final_layer_norm,
            activation_fn: cfg.activation_function,
        })
    }

    fn forward(
        &mut self,
        xs: &Tensor,
        attention_mask: &Tensor,
        encoder_hidden_states: Option<&Tensor>,
    ) -> Result<Tensor> {
        let residual = xs.clone();
        let xs = self.self_attn_layer_norm.forward(xs)?;
        let xs = self.self_attn.forward(&xs, None, attention_mask)?;
        let mut xs = (xs + residual)?;
        if let Some(encoder_hidden_states) = &encoder_hidden_states {
            let residual = xs.clone();
            let encoder_attention_mask = attention_mask.clone(); // TODO
            xs = self.encoder_attn.forward(
                &xs,
                Some(encoder_hidden_states),
                &encoder_attention_mask,
            )?;
            xs = (xs + residual)?
        }
        let residual = xs.clone();
        let xs = self.final_layer_norm.forward(&xs)?;
        let xs = self.fc1.forward(&xs)?;
        let xs = self.activation_fn.forward(&xs)?;
        let xs = self.fc2.forward(&xs)?;
        let xs = (xs + residual)?;
        Ok(xs)
    }
}

#[derive(Debug)]
struct MusicgenDecoder {
    embed_tokens: Vec<Embedding>,
    embed_positions: MusicgenSinusoidalPositionalEmbedding,
    layers: Vec<MusicgenDecoderLayer>,
    layer_norm: LayerNorm,
    embed_scale: f64,
    num_codebooks: usize,
    d_model: usize,
}

impl MusicgenDecoder {
    fn load(p: &str, vb: &VarBuilder, cfg: &Config) -> Result<Self> {
        let h = cfg.hidden_size;
        let embed_scale = if cfg.scale_embedding {
            (h as f64).sqrt()
        } else {
            1.
        };
        let embed_dim = cfg.vocab_size + 1;
        let embed_tokens = (0..cfg.num_codebooks)
            .map(|i| Embedding::load(embed_dim, h, &format!("{p}.embed_tokens.{i}"), vb))
            .collect::<Result<Vec<_>>>()?;
        let embed_positions = MusicgenSinusoidalPositionalEmbedding::load(vb, cfg)?;
        let layers = (0..cfg.num_hidden_layers)
            .map(|i| MusicgenDecoderLayer::load(&format!("{p}.layers.{i}"), vb, cfg))
            .collect::<Result<Vec<_>>>()?;
        let layer_norm = LayerNorm::load(h, 1e-5, &format!("{p}.layer_norm"), vb)?;
        Ok(Self {
            embed_tokens,
            embed_positions,
            layers,
            layer_norm,
            embed_scale,
            num_codebooks: cfg.num_codebooks,
            d_model: cfg.hidden_size,
        })
    }

    fn prepare_decoder_attention_mask(&self, _b_sz: usize, _seq_len: usize) -> Result<Tensor> {
        todo!()
    }

    fn forward(&mut self, input_ids: &Tensor) -> Result<Tensor> {
        let dev = input_ids.device();
        let (b_sz_times_codebooks, seq_len) = input_ids.shape().r2()?;
        let b_sz = b_sz_times_codebooks / self.num_codebooks;
        let input = input_ids.reshape((b_sz, self.num_codebooks, seq_len))?;
        let mut inputs_embeds = Tensor::zeros((b_sz, seq_len, self.d_model), DType::F32, &dev)?;
        for (idx, codebook) in self.embed_tokens.iter().enumerate() {
            let inp = input.narrow(1, idx, 1)?.squeeze(1)?;
            inputs_embeds = (inputs_embeds + codebook.forward(&inp)?)?
        }
        let inputs_embeds = inputs_embeds;
        let positions = self.embed_positions.forward(&input)?.to_device(&dev)?;
        let mut xs = inputs_embeds.broadcast_add(&positions)?;
        let attention_mask = self.prepare_decoder_attention_mask(b_sz, seq_len)?;
        for (_layer_idx, decoder_layer) in self.layers.iter_mut().enumerate() {
            xs = decoder_layer.forward(&xs, &attention_mask, None)?;
        }
        let xs = self.layer_norm.forward(&xs)?;
        Ok(xs)
    }
}

#[derive(Debug)]
pub struct MusicgenForCausalLM {
    decoder: MusicgenDecoder,
    lm_heads: Vec<Linear>,
    num_codebooks: usize,
    vocab_size: usize,
}

impl MusicgenForCausalLM {
    pub fn load(p: &str, vb: &VarBuilder, cfg: &Config) -> Result<Self> {
        let h = cfg.hidden_size;
        let decoder = MusicgenDecoder::load(&format!("{p}.model.decoder"), vb, cfg)?;
        let lm_heads = (0..cfg.num_codebooks)
            .map(|i| Linear::load(h, cfg.vocab_size, false, &format!("{p}.lm_heads.{i}"), vb))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            decoder,
            lm_heads,
            num_codebooks: cfg.num_codebooks,
            vocab_size: cfg.vocab_size,
        })
    }

    pub fn forward(&mut self, input_ids: &Tensor) -> Result<Tensor> {
        let (b_sz, seq_len) = input_ids.shape().r2()?;
        let hidden_states = self.decoder.forward(input_ids)?;
        let lm_logits = self
            .lm_heads
            .iter()
            .map(|h| Ok(h.forward(&hidden_states)?))
            .collect::<Result<Vec<_>>>()?;
        let lm_logits = Tensor::stack(&lm_logits, 1)?.reshape((
            b_sz * self.num_codebooks,
            seq_len,
            self.vocab_size,
        ))?;
        Ok(lm_logits)
    }
}

#[derive(Debug)]
pub struct MusicgenForConditionalGeneration {
    text_encoder: crate::t5_model::T5EncoderModel,
    audio_encoder: crate::encodec_model::EncodecModel,
    decoder: MusicgenForCausalLM,
    cfg: GenConfig,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GenConfig {
    musicgen: Config,
    t5: crate::t5_model::Config,
    encodec: crate::encodec_model::Config,
}

impl GenConfig {
    pub fn small() -> Self {
        Self {
            musicgen: Config::musicgen_small(),
            t5: t5_model::Config::musicgen_small(),
            encodec: encodec_model::Config::musicgen_small(),
        }
    }
}

impl MusicgenForConditionalGeneration {
    pub fn config(&self) -> &GenConfig {
        &self.cfg
    }

    pub fn load(vb: &VarBuilder, cfg: GenConfig) -> Result<Self> {
        let text_encoder = t5_model::T5EncoderModel::load("text_encoder", vb, &cfg.t5)?;
        let audio_encoder = encodec_model::EncodecModel::load("audio_encoder", vb, &cfg.encodec)?;
        let decoder = MusicgenForCausalLM::load("decoder", vb, &cfg.musicgen)?;
        Ok(Self {
            text_encoder,
            audio_encoder,
            decoder,
            cfg,
        })
    }
}
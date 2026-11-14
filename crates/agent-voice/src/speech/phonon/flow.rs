//! The flow-matching language model: conditioned on the voice and the text, it emits one Mimi
//! latent a frame and the logit that says when speech ends.

use super::transformer::{Gelu, Kind, State, Transformer, TransformerShape, layer_norm, linear};
use candle_core::{D, DType, Module, Result, Tensor};
use candle_nn::{LayerNorm, Linear, VarBuilder};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeRmsNorm {
    Rms,
    #[default]
    RmsVar,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct FlowConfig {
    pub d_model: usize,
    pub num_heads: usize,
    pub num_layers: usize,
    pub dim_feedforward: usize,
    pub max_period: f32,
    pub n_bins: usize,
    pub lut_dim: usize,
    pub flow_dim: usize,
    pub flow_depth: usize,
    pub ldim: usize,
    #[serde(default)]
    pub gelu: Option<Gelu>,
    #[serde(default)]
    pub time_rms_norm: TimeRmsNorm,
}

fn modulate(x: &Tensor, shift: &Tensor, scale: &Tensor) -> Result<Tensor> {
    x.broadcast_mul(&(scale + 1.0)?)?.broadcast_add(shift)
}

struct TimestepEmbedder {
    linear1: Linear,
    linear2: Linear,
    alpha: Tensor,
    freqs: Tensor,
    norm: TimeRmsNorm,
}

impl TimestepEmbedder {
    fn load(vb: VarBuilder, hidden: usize, freq_size: usize, norm: TimeRmsNorm) -> Result<Self> {
        let mlp = vb.pp("mlp");
        Ok(TimestepEmbedder {
            linear1: linear(mlp.pp("0"), freq_size, hidden, true)?,
            linear2: linear(mlp.pp("2"), hidden, hidden, true)?,
            alpha: mlp.pp("3").get(hidden, "alpha")?,
            freqs: vb.get(freq_size / 2, "freqs")?,
            norm,
        })
    }

    fn forward(&self, t: &Tensor) -> Result<Tensor> {
        let args = t.broadcast_mul(&self.freqs)?;
        let emb = Tensor::cat(&[args.cos()?, args.sin()?], D::Minus1)?;
        let x = self.linear2.forward(&candle_nn::ops::silu(&self.linear1.forward(&emb)?)?)?;
        let n = x.dim(D::Minus1)? as f64;
        let inv = match self.norm {
            TimeRmsNorm::Rms => {
                ((x.sqr()?.sum_keepdim(D::Minus1)? / n)? + 1e-5)?.sqrt()?.recip()?
            }
            TimeRmsNorm::RmsVar => {
                let mean = (x.sum_keepdim(D::Minus1)? / n)?;
                let var = x.broadcast_sub(&mean)?.sqr()?.sum_keepdim(D::Minus1)?;
                ((var / (n - 1.0))? + 1e-5)?.sqrt()?.recip()?
            }
        };
        x.broadcast_mul(&inv)?.broadcast_mul(&self.alpha)
    }
}

struct ResBlock {
    in_ln: LayerNorm,
    mlp1: Linear,
    mlp2: Linear,
    ada: Linear,
}

impl ResBlock {
    fn load(vb: VarBuilder, c: usize) -> Result<Self> {
        Ok(ResBlock {
            in_ln: layer_norm(vb.pp("in_ln"), c, 1e-6)?,
            mlp1: linear(vb.pp("mlp").pp("0"), c, c, true)?,
            mlp2: linear(vb.pp("mlp").pp("2"), c, c, true)?,
            ada: linear(vb.pp("adaLN_modulation").pp("1"), c, 3 * c, true)?,
        })
    }

    fn forward(&self, x: &Tensor, y: &Tensor) -> Result<Tensor> {
        let c = x.dim(D::Minus1)?;
        let ada = self.ada.forward(&candle_nn::ops::silu(y)?)?;
        let (shift, scale, gate) = (
            ada.narrow(D::Minus1, 0, c)?,
            ada.narrow(D::Minus1, c, c)?,
            ada.narrow(D::Minus1, 2 * c, c)?,
        );
        let h = modulate(&self.in_ln.forward(x)?, &shift, &scale)?;
        let h = self.mlp2.forward(&candle_nn::ops::silu(&self.mlp1.forward(&h)?)?)?;
        x + gate.broadcast_mul(&h)?
    }
}

struct FinalLayer {
    norm: LayerNorm,
    linear: Linear,
    ada: Linear,
}

impl FinalLayer {
    fn load(vb: VarBuilder, c: usize, out: usize) -> Result<Self> {
        let (dt, dev) = (vb.dtype(), vb.device().clone());
        Ok(FinalLayer {
            norm: LayerNorm::new(Tensor::ones(c, dt, &dev)?, Tensor::zeros(c, dt, &dev)?, 1e-6),
            linear: linear(vb.pp("linear"), c, out, true)?,
            ada: linear(vb.pp("adaLN_modulation").pp("1"), c, 2 * c, true)?,
        })
    }

    fn forward(&self, x: &Tensor, c: &Tensor) -> Result<Tensor> {
        let w = x.dim(D::Minus1)?;
        let ada = self.ada.forward(&candle_nn::ops::silu(c)?)?;
        let x = modulate(
            &self.norm.forward(x)?,
            &ada.narrow(D::Minus1, 0, w)?,
            &ada.narrow(D::Minus1, w, w)?,
        )?;
        self.linear.forward(&x)
    }
}

/// The small network that turns the transformer's output and a noise sample into a latent.
struct FlowNet {
    time: Vec<TimestepEmbedder>,
    cond: Linear,
    input: Linear,
    blocks: Vec<ResBlock>,
    last: FinalLayer,
}

impl FlowNet {
    fn load(vb: VarBuilder, cfg: &FlowConfig) -> Result<Self> {
        let c = cfg.flow_dim;
        Ok(FlowNet {
            time: (0..2)
                .map(|i| {
                    TimestepEmbedder::load(vb.pp("time_embed").pp(i), c, 256, cfg.time_rms_norm)
                })
                .collect::<Result<_>>()?,
            cond: linear(vb.pp("cond_embed"), cfg.d_model, c, true)?,
            input: linear(vb.pp("input_proj"), cfg.ldim, c, true)?,
            blocks: (0..cfg.flow_depth)
                .map(|i| ResBlock::load(vb.pp("res_blocks").pp(i), c))
                .collect::<Result<_>>()?,
            last: FinalLayer::load(vb.pp("final_layer"), c, cfg.ldim)?,
        })
    }

    fn forward(&self, cond: &Tensor, ts: [&Tensor; 2], x: &Tensor) -> Result<Tensor> {
        let mut x = self.input.forward(x)?;
        let t = ((self.time[0].forward(ts[0])? + self.time[1].forward(ts[1])?)? / 2.0)?;
        let y = (t + self.cond.forward(cond)?)?;
        for b in &self.blocks {
            x = b.forward(&x, &y)?;
        }
        self.last.forward(&x, &y)
    }

    /// Lagrangian self distillation: from noise to a latent in `steps` straight hops.
    fn decode(&self, cond: &Tensor, noise: &Tensor, steps: usize) -> Result<Tensor> {
        let mut x = noise.clone();
        let mut shape = noise.dims().to_vec();
        *shape.last_mut().unwrap() = 1;
        for i in 0..steps {
            let s = Tensor::full(i as f32 / steps as f32, shape.clone(), noise.device())?
                .to_dtype(noise.dtype())?;
            let t = Tensor::full((i + 1) as f32 / steps as f32, shape.clone(), noise.device())?
                .to_dtype(noise.dtype())?;
            let dir = self.forward(cond, [&s, &t], &x)?;
            x = (x + (dir / steps as f64)?)?;
        }
        Ok(x)
    }
}

pub struct FlowLm {
    /// The text tokens' embeddings, already projected to the model width.
    text_embed: Tensor,
    flow: FlowNet,
    pub transformer: Transformer,
    emb_std: Tensor,
    emb_mean: Tensor,
    bos: Tensor,
    input: Linear,
    out_norm: LayerNorm,
    out_eos: Linear,
    pub dim: usize,
    pub ldim: usize,
}

impl FlowLm {
    pub fn load(vb: VarBuilder, cfg: &FlowConfig, gelu: Gelu) -> Result<Self> {
        let cond = vb.pp("conditioner");
        let text_embed = cond.get((cfg.n_bins + 1, cfg.lut_dim), "embed.weight")?;
        let text_embed = if cond.contains_tensor("output_proj.weight") {
            linear(cond.pp("output_proj"), cfg.lut_dim, cfg.d_model, false)?.forward(&text_embed)?
        } else {
            text_embed
        };
        let transformer = Transformer::load(
            vb.pp("transformer"),
            &TransformerShape {
                dim: cfg.d_model,
                heads: cfg.num_heads,
                layers: cfg.num_layers,
                ff: cfg.dim_feedforward,
                max_period: cfg.max_period,
                layer_scale: false,
                kind: Kind::FlowLm,
                gelu: cfg.gelu.unwrap_or(gelu),
            },
        )?;
        Ok(FlowLm {
            text_embed,
            flow: FlowNet::load(vb.pp("flow_net"), cfg)?,
            transformer,
            emb_std: vb.get(cfg.ldim, "emb_std")?,
            emb_mean: vb.get(cfg.ldim, "emb_mean")?,
            bos: vb.get(cfg.ldim, "bos_emb")?,
            input: linear(vb.pp("input_linear"), cfg.ldim, cfg.d_model, false)?,
            out_norm: layer_norm(vb.pp("out_norm"), cfg.d_model, 1e-5)?,
            out_eos: linear(vb.pp("out_eos"), cfg.d_model, 1, true)?,
            dim: cfg.d_model,
            ldim: cfg.ldim,
        })
    }

    /// `[1, n, dim]` for the token ids. Ids past the table come from a tokenizer that does not
    /// belong to this checkpoint, and are an error rather than a panic.
    pub fn embed_tokens(&self, ids: &[u32]) -> Result<Tensor> {
        let dev = self.text_embed.device();
        if ids.is_empty() {
            return Tensor::zeros((1, 0, self.dim), self.text_embed.dtype(), dev);
        }
        let rows = self.text_embed.dim(0)?;
        if let Some(id) = ids.iter().find(|&&i| i as usize >= rows) {
            candle_core::bail!(
                "token {id} is past the {rows}-entry table: the tokenizer is not this model's"
            )
        }
        let ids = Tensor::new(ids, dev)?;
        self.text_embed.index_select(&ids, 0)?.unsqueeze(0)
    }

    /// Run `prefix` (`[1, n, dim]`, already embedded) through the transformer, filling the cache.
    pub fn prompt(&self, state: &mut State, prefix: &Tensor) -> Result<()> {
        self.transformer.forward(prefix, state).map(drop)
    }

    /// One step: the latent before this one (`None` for the first) in, the next latent and its
    /// end-of-speech logit out. `noise` is `[1, ldim]`.
    pub fn step(
        &self,
        state: &mut State,
        previous: Option<&Tensor>,
        conditions: Option<&Tensor>,
        noise: &Tensor,
        steps: usize,
    ) -> Result<(Tensor, f32)> {
        let x = match previous {
            Some(p) => p.clone(),
            None => self.bos.reshape((1, 1, self.ldim))?,
        };
        let x = self.input.forward(&x)?;
        let x = match conditions {
            Some(c) => x.broadcast_add(c)?,
            None => x,
        };
        let out = self.transformer.forward(&x, state)?;
        let out = self.out_norm.forward(&out)?;
        let last = out.narrow(1, out.dim(1)? - 1, 1)?.reshape((1, self.dim))?;
        let eos =
            self.out_eos.forward(&last)?.flatten_all()?.to_dtype(DType::F32)?.to_vec1::<f32>()?[0];
        let latent = self.flow.decode(&last, noise, steps)?;
        Ok((latent.reshape((1, 1, self.ldim))?, eos))
    }

    /// A latent back in the codec's own scale, `[1, ldim, t]` for the decoder.
    pub fn denormalize(&self, latent: &Tensor) -> Result<Tensor> {
        latent
            .broadcast_mul(&self.emb_std)?
            .broadcast_add(&self.emb_mean)?
            .transpose(1, 2)?
            .contiguous()
    }
}

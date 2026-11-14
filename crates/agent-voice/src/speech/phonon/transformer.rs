//! The streaming transformer the flow LM and Mimi's decoder are both built on.
//!
//! The flow LM keeps every key it has seen; Mimi keeps the last `context` of them. Both rotate
//! queries and keys the same way, pairing dimensions 2i and 2i+1 (candle's `rope_i`).

use candle_core::{Device, Module, Result, Tensor};
use candle_nn::{LayerNorm, Linear, VarBuilder};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gelu {
    Tanh,
    #[default]
    Erf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Mimi { context: usize },
    FlowLm,
}

pub fn linear(vb: VarBuilder, input: usize, output: usize, bias: bool) -> Result<Linear> {
    let w = vb.get((output, input), "weight")?;
    let b = if bias { Some(vb.get(output, "bias")?) } else { None };
    Ok(Linear::new(w, b))
}

pub fn layer_norm(vb: VarBuilder, dim: usize, eps: f64) -> Result<LayerNorm> {
    Ok(LayerNorm::new(vb.get(dim, "weight")?, vb.get(dim, "bias")?, eps))
}

/// The keys and values a layer has seen, and how far into the sequence it is.
#[derive(Clone, Debug)]
pub struct Cache {
    k: Option<Tensor>,
    v: Option<Tensor>,
    /// Positions seen, which the rotation counts from.
    pub offset: usize,
    /// Keys kept, at most; `None` keeps them all.
    context: Option<usize>,
}

impl Cache {
    fn new(context: Option<usize>) -> Self {
        Cache { k: None, v: None, offset: 0, context }
    }

    pub fn len(&self) -> usize {
        self.k.as_ref().map_or(0, |k| k.dim(2).unwrap_or(0))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Add `k` and `v` (`[b, h, t, d]`) and hand back everything to attend over.
    fn append(&mut self, k: &Tensor, v: &Tensor) -> Result<(Tensor, Tensor)> {
        self.offset += k.dim(2)?;
        let (k, v) = match (&self.k, &self.v) {
            (Some(pk), Some(pv)) => (Tensor::cat(&[pk, k], 2)?, Tensor::cat(&[pv, v], 2)?),
            _ => (k.clone(), v.clone()),
        };
        self.k = Some(k.clone());
        self.v = Some(v.clone());
        Ok((k, v))
    }

    fn trim(&mut self) -> Result<()> {
        let Some(context) = self.context else { return Ok(()) };
        let len = self.len();
        if len > context {
            let cut = len - context;
            self.k = Some(self.k.as_ref().unwrap().narrow(2, cut, context)?.contiguous()?);
            self.v = Some(self.v.as_ref().unwrap().narrow(2, cut, context)?.contiguous()?);
        }
        Ok(())
    }
}

pub struct Rope {
    cos: Tensor,
    sin: Tensor,
}

impl Rope {
    pub fn new(
        head_dim: usize,
        offset: usize,
        len: usize,
        max_period: f32,
        dev: &Device,
    ) -> Result<Self> {
        let half = head_dim / 2;
        let inv: Vec<f32> =
            (0..half).map(|i| 1.0 / max_period.powf(i as f32 / half as f32)).collect();
        let mut cos = Vec::with_capacity(len * half);
        let mut sin = Vec::with_capacity(len * half);
        for p in 0..len {
            let p = (p + offset) as f32;
            for f in &inv {
                cos.push((p * f).cos());
                sin.push((p * f).sin());
            }
        }
        Ok(Rope {
            cos: Tensor::from_vec(cos, (len, half), dev)?,
            sin: Tensor::from_vec(sin, (len, half), dev)?,
        })
    }

    /// `x` is `[b, h, t, d]`.
    fn apply(&self, x: &Tensor) -> Result<Tensor> {
        let dt = x.dtype();
        candle_nn::rotary_emb::rope_i(
            &x.contiguous()?,
            &self.cos.to_dtype(dt)?,
            &self.sin.to_dtype(dt)?,
        )
    }
}

struct Attention {
    in_proj: Linear,
    out_proj: Linear,
    dim: usize,
    heads: usize,
    kind: Kind,
}

impl Attention {
    fn load(vb: VarBuilder, dim: usize, heads: usize, kind: Kind) -> Result<Self> {
        Ok(Attention {
            in_proj: linear(vb.pp("in_proj"), dim, 3 * dim, false)?,
            out_proj: linear(vb.pp("out_proj"), dim, dim, false)?,
            dim,
            heads,
            kind,
        })
    }

    fn forward(
        &self,
        x: &Tensor,
        rope: &Rope,
        cache: &mut Cache,
        mask: Option<&Tensor>,
    ) -> Result<Tensor> {
        let (b, t, _) = x.dims3()?;
        let d = self.dim / self.heads;
        let qkv = self.in_proj.forward(x)?;
        // The two models pack q, k and v differently: the flow LM as [q | k | v] along the
        // width, Mimi as three interleaved [h, d] blocks per position.
        let (q, k, v) = match self.kind {
            Kind::FlowLm => {
                let part = |i: usize| -> Result<Tensor> {
                    qkv.narrow(2, i * self.dim, self.dim)?.reshape((b, t, self.heads, d))
                };
                (part(0)?, part(1)?, part(2)?)
            }
            Kind::Mimi { .. } => {
                let packed = qkv.reshape((b, t, 3, self.heads, d))?;
                let part = |i: usize| -> Result<Tensor> {
                    packed.narrow(2, i, 1)?.reshape((b, t, self.heads, d))
                };
                (part(0)?, part(1)?, part(2)?)
            }
        };
        let q = rope.apply(&q.transpose(1, 2)?)?;
        let k = rope.apply(&k.transpose(1, 2)?)?;
        let v = v.transpose(1, 2)?.contiguous()?;
        let (k, v) = cache.append(&k, &v)?;
        let scale = 1.0 / (d as f64).sqrt();
        let att = (q.matmul(&k.t()?)? * scale)?;
        let att = match mask {
            Some(m) => att.broadcast_add(m)?,
            None => att,
        };
        let att = candle_nn::ops::softmax_last_dim(&att)?;
        let y = att.matmul(&v)?;
        cache.trim()?;
        let y = y.transpose(1, 2)?.reshape((b, t, self.dim))?;
        self.out_proj.forward(&y)
    }
}

struct Layer {
    attn: Attention,
    norm1: LayerNorm,
    norm2: LayerNorm,
    linear1: Linear,
    linear2: Linear,
    scale1: Option<Tensor>,
    scale2: Option<Tensor>,
    gelu: Gelu,
}

/// `x sigmoid(2u)`, `u = sqrt(2/pi)(x + 0.044715 x^3)`: the tanh GELU, as ptts computes it.
fn gelu_tanh(x: &Tensor) -> Result<Tensor> {
    let u2 = ((x.sqr()? * 0.071_354_82)? + 1.595_769)?.mul(x)?;
    candle_nn::ops::sigmoid(&u2)?.mul(x)
}

impl Layer {
    #[allow(clippy::too_many_arguments)]
    fn load(
        vb: VarBuilder,
        dim: usize,
        heads: usize,
        ff: usize,
        layer_scale: bool,
        kind: Kind,
        gelu: Gelu,
    ) -> Result<Self> {
        let scale = |name: &str| -> Result<Option<Tensor>> {
            if layer_scale { Ok(Some(vb.pp(name).get(dim, "scale")?)) } else { Ok(None) }
        };
        Ok(Layer {
            attn: Attention::load(vb.pp("self_attn"), dim, heads, kind)?,
            norm1: layer_norm(vb.pp("norm1"), dim, 1e-5)?,
            norm2: layer_norm(vb.pp("norm2"), dim, 1e-5)?,
            linear1: linear(vb.pp("linear1"), dim, ff, false)?,
            linear2: linear(vb.pp("linear2"), ff, dim, false)?,
            scale1: scale("layer_scale_1")?,
            scale2: scale("layer_scale_2")?,
            gelu,
        })
    }

    fn forward(
        &self,
        x: &Tensor,
        rope: &Rope,
        cache: &mut Cache,
        mask: Option<&Tensor>,
    ) -> Result<Tensor> {
        let mut a = self.attn.forward(&self.norm1.forward(x)?, rope, cache, mask)?;
        if let Some(s) = &self.scale1 {
            a = a.broadcast_mul(s)?;
        }
        let x = (x + a)?;
        let h = self.linear1.forward(&self.norm2.forward(&x)?)?;
        let h = match self.gelu {
            Gelu::Tanh => gelu_tanh(&h)?,
            Gelu::Erf => h.gelu_erf()?,
        };
        let mut f = self.linear2.forward(&h)?;
        if let Some(s) = &self.scale2 {
            f = f.broadcast_mul(s)?;
        }
        x + f
    }
}

pub struct Transformer {
    layers: Vec<Layer>,
    head_dim: usize,
    max_period: f32,
    kind: Kind,
}

#[derive(Clone, Debug)]
pub struct State {
    pub layers: Vec<Cache>,
}

pub struct TransformerShape {
    pub dim: usize,
    pub heads: usize,
    pub layers: usize,
    pub ff: usize,
    pub max_period: f32,
    pub layer_scale: bool,
    pub kind: Kind,
    pub gelu: Gelu,
}

impl Transformer {
    pub fn load(vb: VarBuilder, s: &TransformerShape) -> Result<Self> {
        if s.heads == 0 || !s.dim.is_multiple_of(s.heads) {
            candle_core::bail!("{} attention heads do not divide the width {}", s.heads, s.dim)
        }
        let layers = (0..s.layers)
            .map(|i| {
                Layer::load(
                    vb.pp("layers").pp(i),
                    s.dim,
                    s.heads,
                    s.ff,
                    s.layer_scale,
                    s.kind,
                    s.gelu,
                )
            })
            .collect::<Result<_>>()?;
        Ok(Transformer {
            layers,
            head_dim: s.dim / s.heads,
            max_period: s.max_period,
            kind: s.kind,
        })
    }

    pub fn init_state(&self) -> State {
        let context = match self.kind {
            Kind::Mimi { context } => Some(context),
            Kind::FlowLm => None,
        };
        State { layers: self.layers.iter().map(|_| Cache::new(context)).collect() }
    }

    /// The causal mask for `t` new positions after `seen` cached ones. Mimi also hides keys
    /// further back than its context.
    fn mask(&self, seen: usize, t: usize, dev: &Device) -> Result<Option<Tensor>> {
        if t == 1 {
            return Ok(None);
        }
        let keys = seen + t;
        let context = match self.kind {
            Kind::Mimi { context } => Some(context),
            Kind::FlowLm => None,
        };
        let mut m = Vec::with_capacity(t * keys);
        for q in 0..t {
            let at = q + seen;
            for k in 0..keys {
                let far = context.is_some_and(|c| k < at.saturating_sub(c));
                m.push(if k > at || far { f32::NEG_INFINITY } else { 0.0 });
            }
        }
        Ok(Some(Tensor::from_vec(m, (1, 1, t, keys), dev)?))
    }

    pub fn forward(&self, x: &Tensor, state: &mut State) -> Result<Tensor> {
        let (_, t, _) = x.dims3()?;
        let first = state.layers.first();
        let seen = first.map_or(0, |c| c.len());
        let offset = first.map_or(0, |c| c.offset);
        let mask = self.mask(seen, t, x.device())?.map(|m| m.to_dtype(x.dtype())).transpose()?;
        let rope = Rope::new(self.head_dim, offset, t, self.max_period, x.device())?;
        let mut x = x.clone();
        for (layer, cache) in self.layers.iter().zip(state.layers.iter_mut()) {
            x = layer.forward(&x, &rope, cache, mask.as_ref())?;
        }
        Ok(x)
    }
}

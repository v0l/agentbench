//! The fixed conditions a checkpoint is trained with, summed into one vector added to every
//! frame: how many speakers, how much faster or slower than usual. The defaults are what a
//! request that says nothing gets.

use super::transformer::linear;
use candle_core::{Module, Result, Tensor};
use candle_nn::VarBuilder;

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ConditionerConfig {
    Lut { name: String, lut: LutConfig },
    Continuous { name: String, continuous: ContinuousConfig },
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct LutConfig {
    pub n_bins: usize,
    pub dim: usize,
    pub tokenizer: String,
    #[serde(default)]
    pub possible_values: Option<Vec<String>>,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct ContinuousConfig {
    pub scale_factor: f32,
    pub dim: usize,
    #[serde(default = "max_period")]
    pub max_period: f32,
}

fn max_period() -> f32 {
    10000.0
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct FuserConfig {
    #[serde(default)]
    pub sum: Vec<String>,
}

fn default_value(name: &str) -> Option<&'static str> {
    match name {
        "num_speakers" => Some("1"),
        "duration_delta" | "padding_bonus" => Some("0.0"),
        _ => None,
    }
}

/// `dim` values, cosines then sines, of `position` over geometric periods up to `max_period`.
pub fn sin_embedding(position: f32, dim: usize, max_period: f32) -> Result<Vec<f32>> {
    if dim < 4 || !dim.is_multiple_of(2) {
        candle_core::bail!("a sinusoidal embedding needs an even dim of at least 4, got {dim}")
    }
    let half = dim / 2;
    let phase: Vec<f32> =
        (0..half).map(|i| position / max_period.powf(i as f32 / (half - 1) as f32)).collect();
    Ok(phase.iter().map(|p| p.cos()).chain(phase.iter().map(|p| p.sin())).collect())
}

/// The summed conditions at their defaults, `[1, 1, d_model]`, or `None` for a checkpoint with
/// none.
pub fn defaults(
    vb: VarBuilder,
    conditioners: &[ConditionerConfig],
    fuser: Option<&FuserConfig>,
    d_model: usize,
) -> Result<Option<Tensor>> {
    let mut sum: Option<Tensor> = None;
    for cfg in conditioners {
        let (name, cond) = match cfg {
            ConditionerConfig::Lut { name, lut } => {
                if lut.tokenizer != "noop" {
                    candle_core::bail!(
                        "unsupported tokenizer {:?} for the {name} conditioner",
                        lut.tokenizer
                    )
                }
                let values = lut.possible_values.as_ref().ok_or_else(|| {
                    candle_core::Error::msg(format!("the {name} conditioner lists no values"))
                })?;
                let value = default_value(name).ok_or_else(|| {
                    candle_core::Error::msg(format!("the {name} conditioner has no default"))
                })?;
                let id = values.iter().position(|v| v == value).ok_or_else(|| {
                    candle_core::Error::msg(format!("{value:?} is not one of {name}'s values"))
                })?;
                let vb = vb.pp(name);
                let table = vb.get((lut.n_bins + 1, lut.dim), "embed.weight")?;
                let row = table.narrow(0, id, 1)?.unsqueeze(0)?;
                let row = if vb.contains_tensor("output_proj.weight") {
                    linear(vb.pp("output_proj"), lut.dim, d_model, false)?.forward(&row)?
                } else {
                    row
                };
                (name, row)
            }
            ConditionerConfig::Continuous { name, continuous: c } => {
                let value: f32 = default_value(name)
                    .ok_or_else(|| {
                        candle_core::Error::msg(format!("the {name} conditioner has no default"))
                    })?
                    .parse()
                    .map_err(candle_core::Error::wrap)?;
                let emb = sin_embedding(c.scale_factor * value, c.dim, c.max_period)?;
                let emb =
                    Tensor::from_vec(emb, (1, 1, c.dim), vb.device())?.to_dtype(vb.dtype())?;
                let vb = vb.pp(name);
                let emb = if vb.contains_tensor("output_proj.weight") {
                    linear(vb.pp("output_proj"), c.dim, d_model, false)?.forward(&emb)?
                } else if c.dim == d_model {
                    emb
                } else {
                    candle_core::bail!("the {name} conditioner has no projection to {d_model}")
                };
                (name, emb)
            }
        };
        if let Some(f) = fuser
            && !f.sum.iter().any(|s| s == name)
        {
            candle_core::bail!("the {name} conditioner is not summed, the only fusion supported")
        }
        sum = Some(match sum {
            Some(s) => (s + cond)?,
            None => cond,
        });
    }
    Ok(sum)
}

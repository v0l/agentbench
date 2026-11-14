//! Mimi's decoder: latents up to the codec's width, an upsampler to the encoder's frame rate, a
//! small transformer, then SEANet down to samples.

use super::conv::{StreamingConvTr1dState, StreamingConvTranspose1d};
use super::seanet::{Decoder, DecoderShape, DecoderState};
use super::transformer::{Gelu, Kind, State, Transformer, TransformerShape, linear};
use candle_core::{Module, Result, Tensor};
use candle_nn::VarBuilder;

#[derive(Clone, Debug, serde::Deserialize)]
pub struct MimiConfig {
    pub channels: usize,
    pub sample_rate: usize,
    pub frame_rate: f64,
    pub dimension: usize,
    pub quantizer_dimension: usize,
    pub quantizer_output_dimension: usize,
    pub n_filters: usize,
    pub n_residual_layers: usize,
    pub ratios: Vec<usize>,
    pub kernel_size: usize,
    pub last_kernel_size: usize,
    pub residual_kernel_size: usize,
    pub dilation_base: usize,
    pub compress: usize,
    pub transformer_d_model: usize,
    pub transformer_num_heads: usize,
    pub transformer_num_layers: usize,
    pub transformer_layer_scale: f64,
    pub transformer_context: usize,
    pub transformer_max_period: f32,
    pub transformer_dim_feedforward: usize,
    #[serde(default)]
    pub gelu: Option<Gelu>,
}

pub struct Mimi {
    quantizer: Tensor,
    quantizer_bias: Option<Tensor>,
    upsample: Option<StreamingConvTranspose1d>,
    transformer: Transformer,
    input_proj: Option<candle_nn::Linear>,
    output_proj: Option<candle_nn::Linear>,
    decoder: Decoder,
}

#[derive(Clone, Debug)]
pub struct MimiState {
    upsample: Option<StreamingConvTr1dState>,
    transformer: State,
    decoder: DecoderState,
}

impl Mimi {
    pub fn load(vb: VarBuilder, cfg: &MimiConfig, gelu: Gelu) -> Result<Self> {
        let q = vb.pp("quantizer").pp("output_proj");
        let quantizer =
            q.get((cfg.quantizer_output_dimension, cfg.quantizer_dimension, 1), "weight")?;
        let quantizer_bias = if q.contains_tensor("bias") {
            Some(q.get(cfg.quantizer_output_dimension, "bias")?)
        } else {
            None
        };
        let hop: usize = cfg.ratios.iter().product();
        let encoder_rate = cfg.sample_rate as f64 / hop as f64;
        let upsample = if (encoder_rate - cfg.frame_rate).abs() > 0.01 {
            let stride = (encoder_rate / cfg.frame_rate) as usize;
            Some(StreamingConvTranspose1d::load(
                vb.pp("upsample").pp("convtr"),
                cfg.dimension,
                cfg.dimension,
                2 * stride,
                stride,
                cfg.dimension,
                false,
            )?)
        } else {
            None
        };
        let t = vb.pp("decoder_transformer");
        let d = cfg.transformer_d_model;
        let transformer = Transformer::load(
            t.pp("transformer"),
            &TransformerShape {
                dim: d,
                heads: cfg.transformer_num_heads,
                layers: cfg.transformer_num_layers,
                ff: cfg.transformer_dim_feedforward,
                max_period: cfg.transformer_max_period,
                layer_scale: true,
                kind: Kind::Mimi { context: cfg.transformer_context },
                gelu,
            },
        )?;
        let input_proj = if d != cfg.dimension {
            Some(linear(t.pp("input_proj"), cfg.dimension, d, false)?)
        } else {
            None
        };
        let output_proj = if d != cfg.dimension {
            Some(linear(t.pp("output_proj").pp(0), d, cfg.dimension, false)?)
        } else {
            None
        };
        let decoder = Decoder::load(
            vb.pp("decoder"),
            DecoderShape {
                channels: cfg.channels,
                dimension: cfg.dimension,
                n_filters: cfg.n_filters,
                n_residual_layers: cfg.n_residual_layers,
                ratios: &cfg.ratios,
                kernel_size: cfg.kernel_size,
                last_kernel_size: cfg.last_kernel_size,
                residual_kernel_size: cfg.residual_kernel_size,
                dilation_base: cfg.dilation_base,
                compress: cfg.compress,
            },
        )?;
        Ok(Mimi {
            quantizer,
            quantizer_bias,
            upsample,
            transformer,
            input_proj,
            output_proj,
            decoder,
        })
    }

    pub fn init_state(&self, batch: usize) -> Result<MimiState> {
        Ok(MimiState {
            upsample: self.upsample.as_ref().map(|u| u.init_state(batch)).transpose()?,
            transformer: self.transformer.init_state(),
            decoder: self.decoder.init_state(batch)?,
        })
    }

    /// `[1, ldim, t]` latents to `[1, 1, samples]`.
    pub fn decode(&self, z: &Tensor, state: &mut MimiState) -> Result<Tensor> {
        let z = z.conv1d(&self.quantizer, 0, 1, 1, 1)?;
        let z = match &self.quantizer_bias {
            Some(b) => z.broadcast_add(&b.reshape((1, b.elem_count(), 1))?)?,
            None => z,
        };
        let z = match (&self.upsample, &mut state.upsample) {
            (Some(u), Some(s)) => u.forward(&z, s)?,
            _ => z,
        };
        let x = z.transpose(1, 2)?.contiguous()?;
        let x = match &self.input_proj {
            Some(p) => p.forward(&x)?,
            None => x,
        };
        let x = self.transformer.forward(&x, &mut state.transformer)?;
        let x = match &self.output_proj {
            Some(p) => p.forward(&x)?,
            None => x,
        };
        let x = x.transpose(1, 2)?.contiguous()?;
        self.decoder.forward(&x, &mut state.decoder)
    }
}

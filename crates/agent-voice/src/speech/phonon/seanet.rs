//! SEANet, the convolutional half of Mimi's decoder: latents up to samples.

use super::conv::{
    PadMode, StreamingConv1d, StreamingConv1dState, StreamingConvTr1dState,
    StreamingConvTranspose1d,
};
use candle_core::{Result, Tensor};
use candle_nn::VarBuilder;

pub struct ResnetBlock {
    convs: Vec<StreamingConv1d>,
}

#[derive(Clone, Debug)]
pub struct ResnetBlockState {
    convs: Vec<StreamingConv1dState>,
}

impl ResnetBlock {
    fn load(
        vb: VarBuilder,
        dim: usize,
        kernel_sizes: &[usize],
        dilations: &[usize],
        pad_mode: PadMode,
        compress: usize,
    ) -> Result<Self> {
        if compress == 0 || compress > dim {
            candle_core::bail!("SEANet compress is {compress}; it must be from 1 to {dim}")
        }
        let hidden = dim / compress;
        let mut convs = Vec::new();
        for (i, (&k, &d)) in kernel_sizes.iter().zip(dilations).enumerate() {
            let in_c = if i == 0 { dim } else { hidden };
            let out_c = if i == kernel_sizes.len() - 1 { dim } else { hidden };
            convs.push(StreamingConv1d::load(
                vb.pp("block").pp(2 * i + 1),
                in_c,
                out_c,
                k,
                1,
                d,
                pad_mode,
                1,
                true,
            )?);
        }
        Ok(Self { convs })
    }

    fn init_state(&self, batch: usize) -> Result<ResnetBlockState> {
        Ok(ResnetBlockState {
            convs: self.convs.iter().map(|c| c.init_state(batch)).collect::<Result<_>>()?,
        })
    }

    fn forward(&self, x: &Tensor, state: &mut ResnetBlockState) -> Result<Tensor> {
        let mut v = x.clone();
        for (conv, s) in self.convs.iter().zip(state.convs.iter_mut()) {
            v = conv.forward(&v.elu(1.0)?, s)?;
        }
        x + v
    }
}

struct Layer {
    upsample: StreamingConvTranspose1d,
    residuals: Vec<ResnetBlock>,
}

pub struct Decoder {
    init_conv: StreamingConv1d,
    layers: Vec<Layer>,
    final_conv: StreamingConv1d,
}

#[derive(Clone, Debug)]
pub struct DecoderState {
    init_conv: StreamingConv1dState,
    layers: Vec<(StreamingConvTr1dState, Vec<ResnetBlockState>)>,
    final_conv: StreamingConv1dState,
}

#[derive(Clone, Copy)]
pub struct DecoderShape<'a> {
    pub channels: usize,
    pub dimension: usize,
    pub n_filters: usize,
    pub n_residual_layers: usize,
    pub ratios: &'a [usize],
    pub kernel_size: usize,
    pub last_kernel_size: usize,
    pub residual_kernel_size: usize,
    pub dilation_base: usize,
    pub compress: usize,
}

impl Decoder {
    pub fn load(vb: VarBuilder, s: DecoderShape<'_>) -> Result<Self> {
        let pad = PadMode::Constant;
        let model = vb.pp("model");
        let mut mult = 1 << s.ratios.len();
        let init_conv = StreamingConv1d::load(
            model.pp(0),
            s.dimension,
            mult * s.n_filters,
            s.kernel_size,
            1,
            1,
            pad,
            1,
            true,
        )?;
        let mut layers = Vec::new();
        let mut at = 1usize;
        for &ratio in s.ratios {
            let upsample = StreamingConvTranspose1d::load(
                model.pp(at + 1),
                mult * s.n_filters,
                mult * s.n_filters / 2,
                ratio * 2,
                ratio,
                1,
                true,
            )?;
            at += 2;
            let mut residuals = Vec::new();
            for j in 0..s.n_residual_layers {
                residuals.push(ResnetBlock::load(
                    model.pp(at),
                    mult * s.n_filters / 2,
                    &[s.residual_kernel_size, 1],
                    &[s.dilation_base.pow(j as u32), 1],
                    pad,
                    s.compress,
                )?);
                at += 1;
            }
            layers.push(Layer { upsample, residuals });
            mult /= 2;
        }
        let final_conv = StreamingConv1d::load(
            model.pp(at + 1),
            s.n_filters,
            s.channels,
            s.last_kernel_size,
            1,
            1,
            pad,
            1,
            true,
        )?;
        Ok(Self { init_conv, layers, final_conv })
    }

    pub fn init_state(&self, batch: usize) -> Result<DecoderState> {
        let layers = self
            .layers
            .iter()
            .map(|l| {
                Ok((
                    l.upsample.init_state(batch)?,
                    l.residuals.iter().map(|r| r.init_state(batch)).collect::<Result<_>>()?,
                ))
            })
            .collect::<Result<_>>()?;
        Ok(DecoderState {
            init_conv: self.init_conv.init_state(batch)?,
            layers,
            final_conv: self.final_conv.init_state(batch)?,
        })
    }

    pub fn forward(&self, z: &Tensor, state: &mut DecoderState) -> Result<Tensor> {
        let mut z = self.init_conv.forward(z, &mut state.init_conv)?;
        for (layer, (up, res)) in self.layers.iter().zip(state.layers.iter_mut()) {
            z = layer.upsample.forward(&z.elu(1.0)?, up)?;
            for (r, s) in layer.residuals.iter().zip(res.iter_mut()) {
                z = r.forward(&z, s)?;
            }
        }
        self.final_conv.forward(&z.elu(1.0)?, &mut state.final_conv)
    }
}

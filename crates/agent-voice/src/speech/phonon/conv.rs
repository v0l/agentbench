//! The convolutions Mimi's decoder streams through, each keeping what it needs of the last call.

use candle_core::{Module, Result, Tensor};
use candle_nn::VarBuilder;

pub struct Conv1d {
    weight: Tensor,
    bias: Option<Tensor>,
    pub stride: usize,
    pub dilation: usize,
    pub kernel_size: usize,
    pub in_channels: usize,
    pub groups: usize,
}

impl Conv1d {
    #[allow(clippy::too_many_arguments)]
    pub fn load(
        vb: VarBuilder,
        in_channels: usize,
        out_channels: usize,
        kernel_size: usize,
        stride: usize,
        dilation: usize,
        groups: usize,
        bias: bool,
    ) -> Result<Self> {
        let weight = vb.get((out_channels, in_channels / groups, kernel_size), "weight")?;
        let bias = if bias { Some(vb.get(out_channels, "bias")?) } else { None };
        Ok(Self { weight, bias, stride, dilation, kernel_size, in_channels, groups })
    }

    pub fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let y = x.conv1d(&self.weight, 0, self.stride, self.dilation, self.groups)?;
        match &self.bias {
            Some(b) => y.broadcast_add(&b.reshape((1, b.elem_count(), 1))?),
            None => Ok(y),
        }
    }
}

pub struct ConvTranspose1d {
    weight: Tensor,
    pub bias: Option<Tensor>,
    pub stride: usize,
    pub kernel_size: usize,
    pub out_channels: usize,
    pub groups: usize,
}

impl ConvTranspose1d {
    pub fn load(
        vb: VarBuilder,
        in_channels: usize,
        out_channels: usize,
        kernel_size: usize,
        stride: usize,
        groups: usize,
        bias: bool,
    ) -> Result<Self> {
        let weight = vb.get((in_channels, out_channels / groups, kernel_size), "weight")?;
        let bias = if bias { Some(vb.get(out_channels, "bias")?) } else { None };
        Ok(Self { weight, bias, stride, kernel_size, out_channels, groups })
    }

    pub fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let y = if self.groups == 1 {
            x.conv_transpose1d(&self.weight, 0, 0, self.stride, 1, 1)?
        } else {
            depthwise_transpose(x, &self.weight, self.stride, self.groups)?
        };
        match &self.bias {
            Some(b) => y.broadcast_add(&b.reshape((1, b.elem_count(), 1))?),
            None => Ok(y),
        }
    }
}

/// A transposed convolution with one input channel per group, as Mimi's upsampler is. Each
/// output sample is a sum of input samples times kernel taps, so it is built as a tensor product
/// and an overlap-add, which every backend does the same way.
fn depthwise_transpose(
    x: &Tensor,
    weight: &Tensor,
    stride: usize,
    groups: usize,
) -> Result<Tensor> {
    let (b, c, l) = x.dims3()?;
    let (wc, per, k) = weight.dims3()?;
    if wc != c || c != groups || per != 1 {
        candle_core::bail!(
            "depthwise transpose wants one channel per group, got {:?}",
            weight.dims()
        )
    }
    let taps = weight.reshape((1, c, 1, k))?;
    let spread = x.unsqueeze(3)?.broadcast_mul(&taps)?;
    let out_len = (l - 1) * stride + k;
    let mut y = Tensor::zeros((b, c, out_len), x.dtype(), x.device())?;
    if k % stride == 0 {
        let hops = k / stride;
        let frames = spread.reshape((b, c, l, hops, stride))?;
        for h in 0..hops {
            let part = frames.narrow(3, h, 1)?.squeeze(3)?.reshape((b, c, l * stride))?;
            let at = h * stride;
            y = y.slice_assign(
                &[0..b, 0..c, at..at + l * stride],
                &(y.narrow(2, at, l * stride)? + part)?,
            )?;
        }
    } else {
        for i in 0..l {
            let at = i * stride;
            let piece = spread.narrow(2, i, 1)?.squeeze(2)?;
            y = y.slice_assign(&[0..b, 0..c, at..at + k], &(y.narrow(2, at, k)? + piece)?)?;
        }
    }
    Ok(y)
}

#[derive(Clone, Copy)]
pub enum PadMode {
    Constant,
    #[allow(dead_code)]
    Replicate,
}

#[derive(Clone, Debug)]
pub struct StreamingConv1dState {
    pub previous: Tensor,
    pub first: bool,
}

pub struct StreamingConv1d {
    pub conv: Conv1d,
    pad_mode: PadMode,
}

impl StreamingConv1d {
    #[allow(clippy::too_many_arguments)]
    pub fn load(
        vb: VarBuilder,
        in_channels: usize,
        out_channels: usize,
        kernel_size: usize,
        stride: usize,
        dilation: usize,
        pad_mode: PadMode,
        groups: usize,
        bias: bool,
    ) -> Result<Self> {
        let conv = Conv1d::load(
            vb.pp("conv"),
            in_channels,
            out_channels,
            kernel_size,
            stride,
            dilation,
            groups,
            bias,
        )?;
        Ok(Self { conv, pad_mode })
    }

    fn effective_kernel_size(&self) -> usize {
        (self.conv.kernel_size - 1) * self.conv.dilation + 1
    }

    pub fn init_state(&self, batch: usize) -> Result<StreamingConv1dState> {
        let keep = self.effective_kernel_size() - self.conv.stride;
        let previous = Tensor::zeros(
            (batch, self.conv.in_channels, keep),
            self.conv.weight.dtype(),
            self.conv.weight.device(),
        )?;
        Ok(StreamingConv1dState { previous, first: true })
    }

    pub fn forward(&self, x: &Tensor, state: &mut StreamingConv1dState) -> Result<Tensor> {
        let keep = state.previous.dim(2)?;
        if keep > 0 && matches!(self.pad_mode, PadMode::Replicate) && state.first {
            let first = x.narrow(2, 0, 1)?;
            state.previous = first.repeat((1, 1, keep))?;
        }
        let x = if keep > 0 { Tensor::cat(&[&state.previous, x], 2)? } else { x.clone() };
        let y = self.conv.forward(&x)?;
        if keep > 0 {
            let len = x.dim(2)?;
            state.previous = x.narrow(2, len - keep, keep)?.contiguous()?;
            if matches!(self.pad_mode, PadMode::Replicate) {
                state.first = false;
            }
        }
        Ok(y)
    }
}

#[derive(Clone, Debug)]
pub struct StreamingConvTr1dState {
    pub partial: Tensor,
}

pub struct StreamingConvTranspose1d {
    pub convtr: ConvTranspose1d,
}

impl StreamingConvTranspose1d {
    pub fn load(
        vb: VarBuilder,
        in_channels: usize,
        out_channels: usize,
        kernel_size: usize,
        stride: usize,
        groups: usize,
        bias: bool,
    ) -> Result<Self> {
        let convtr = ConvTranspose1d::load(
            vb.pp("convtr"),
            in_channels,
            out_channels,
            kernel_size,
            stride,
            groups,
            bias,
        )?;
        Ok(Self { convtr })
    }

    pub fn init_state(&self, batch: usize) -> Result<StreamingConvTr1dState> {
        let overlap = self.convtr.kernel_size - self.convtr.stride;
        let partial = Tensor::zeros(
            (batch, self.convtr.out_channels, overlap),
            self.convtr.weight.dtype(),
            self.convtr.weight.device(),
        )?;
        Ok(StreamingConvTr1dState { partial })
    }

    pub fn forward(&self, x: &Tensor, state: &mut StreamingConvTr1dState) -> Result<Tensor> {
        let y = self.convtr.forward(x)?;
        let overlap = state.partial.dim(2)?;
        if overlap == 0 {
            return Ok(y);
        }
        let len = y.dim(2)?;
        let head = (y.narrow(2, 0, overlap)? + &state.partial)?;
        let y = Tensor::cat(&[&head, &y.narrow(2, overlap, len - overlap)?], 2)?;
        let mut tail = y.narrow(2, len - overlap, overlap)?;
        if let Some(bias) = &self.convtr.bias {
            tail = tail.broadcast_sub(&bias.reshape((1, bias.elem_count(), 1))?)?;
        }
        state.partial = tail.contiguous()?;
        y.narrow(2, 0, len - overlap)
    }
}

impl Module for Conv1d {
    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        Conv1d::forward(self, x)
    }
}

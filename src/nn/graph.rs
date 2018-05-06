// Copyright 2017 Karl Sundequist Blomdahl <karl.sundequist.blomdahl@gmail.com>
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::mem::size_of;
use std::ptr;
use std::rc::Rc;
use std::sync::Arc;

use libc::c_void;

use go::{FEATURE_SIZE, NUM_FEATURES};
use nn::devices::get_current_device;
use nn::ffi::{cublas, cuda, cudnn};
use nn::slots::*;
use nn::output_map::*;
use nn::tensor::Tensor;
use util::types::f16;

/// The number of features in the neural network architecture.
const NUM_CHANNELS: usize = 128;

/// The number of residual blocks in the neural network architecture.
const NUM_LAYERS: usize = 9;

// A __global__ constant that contains `0.0`.
const ZERO: f32 = 0.0;

// A __global__ constant that contains `1.0`.
const ONE: f32 = 1.0;

// -------- InferenceType --------

pub trait InferenceType: Copy + Default + Sized {
    type Tower: InferenceType;
    type Output: InferenceType;

    fn as_f32(self) -> f32;
}

impl InferenceType for i8 {
    type Tower = i8;
    type Output = f32;

    fn as_f32(self) -> f32 { 6.0 * (self as f32) / 127.0 }
}

impl InferenceType for f16 {
    type Tower = f16;
    type Output = f16;

    fn as_f32(self) -> f32 { f32::from(self) }
}

impl InferenceType for f32 {
    type Tower = f32;
    type Output = f32;

    fn as_f32(self) -> f32 { self }
}

/// 
unsafe fn load_to_host<T: InferenceType>(
    ptr: *const c_void,
    num_elements: usize,
    stream: cuda::Stream
) -> Vec<f32>
{
    let mut host = vec! [T::default(); num_elements];

    check!(cuda::cudaMemcpyAsync(
        host.as_mut_ptr() as *mut c_void,
        ptr,
        size_of::<T>() * num_elements,
        cuda::MemcpyKind::DeviceToHost,
        stream
    ));
    check!(cuda::cudaStreamSynchronize(stream));

    host.into_iter().map(|x| x.as_f32()).collect()
}

// -------- Graph --------

pub struct Builder {
    tensors: Arc<HashMap<String, Tensor>>,
    slots: Slots
}

impl Builder {
    pub fn new(tensors: HashMap<String, Tensor>) -> Builder {
        Builder {
            tensors: Arc::new(tensors),
            slots: Slots::new()
        }
    }

    /// Returns a mutable workspace that contains everything you need to
    /// perform a forward pass through the network pre-allocated.
    /// 
    /// # Arguments
    /// 
    /// * `batch_size` - 
    /// 
    pub fn get_workspace(&self, batch_size: usize) -> Workspace {
        let mut handle_dnn: cudnn::Handle = ptr::null();

        unsafe {
            check!(cudnn::cudnnCreate(&mut handle_dnn));
        }

        let mut w = Workspace {
            batch_size: batch_size,
            tensors: self.tensors.clone(),
            slots: self.slots.clone(),

            handle_blas: ptr::null(),
            handle_dnn: handle_dnn,

            tower_finished: ptr::null(),

            tower_stream: ptr::null(),
            policy_stream: ptr::null(),
            value_stream: ptr::null(),

            c_up: unsafe { Rc::new(UpLayer::new(&handle_dnn, batch_size as i32, &self.tensors)) },
            c_value: unsafe { Rc::new(ValueLayer::new(&handle_dnn, batch_size as i32, &self.tensors)) },
            c_policy: unsafe { Rc::new(PolicyLayer::new(&handle_dnn, batch_size as i32, &self.tensors)) },
            c_residual: (0..NUM_LAYERS).map(|i| unsafe {
                Rc::new(ResidualLayer::new(&handle_dnn, batch_size as i32, i, &self.tensors))
            }).collect()
        };

        unsafe {
            check!(cublas::cublasCreate_v2(&mut w.handle_blas));
            check!(cuda::cudaStreamCreateWithFlags(&mut w.tower_stream, 1));
            check!(cuda::cudaStreamCreateWithFlags(&mut w.policy_stream, 1));
            check!(cuda::cudaStreamCreateWithFlags(&mut w.value_stream, 1));
            check!(cuda::cudaEventCreateWithFlags(&mut w.tower_finished, 2));

            #[cfg(feature = "tensor-core")] {
                check!(cublas::cublasSetMathMode(w.handle_blas, cublas::Math::TensorOp));
            }
        }

        w
    }
}

pub struct Workspace {
    batch_size: usize,
    tensors: Arc<HashMap<String, Tensor>>,
    slots: Slots,

    handle_dnn: cudnn::Handle,
    handle_blas: cublas::Handle,

    tower_finished: cuda::Event,

    tower_stream: cuda::Stream,
    policy_stream: cuda::Stream,
    value_stream: cuda::Stream,

    c_up: Rc<UpLayer>,
    c_value: Rc<ValueLayer>,
    c_policy: Rc<PolicyLayer>,
    c_residual: Vec<Rc<ResidualLayer>>
}

impl Workspace {
    // pass
}

impl Drop for Workspace {
    fn drop(&mut self) {
        unsafe {
            cuda::cudaEventDestroy(self.tower_finished);

            cuda::cudaStreamDestroy(self.value_stream);
            cuda::cudaStreamDestroy(self.policy_stream);
            cuda::cudaStreamDestroy(self.tower_stream);

            cublas::cublasDestroy_v2(self.handle_blas);
            cudnn::cudnnDestroy(self.handle_dnn);
        }
    }
}

struct UpLayer {
    input: cudnn::TensorDescriptor,
    output: cudnn::TensorDescriptor,
    offset: cudnn::TensorDescriptor,
    filter: cudnn::FilterDescriptor,
    relu: cudnn::ActivationDescriptor,
    descr: cudnn::ConvolutionDescriptor,
    fwd_algo: cudnn::ConvolutionFwdAlgoPerf,

    alpha: f32
}

impl Drop for UpLayer {
    fn drop(&mut self) {
        unsafe {
            cudnn::cudnnDestroyTensorDescriptor(self.input);
            cudnn::cudnnDestroyTensorDescriptor(self.output);
            cudnn::cudnnDestroyTensorDescriptor(self.offset);
            cudnn::cudnnDestroyFilterDescriptor(self.filter);
            cudnn::cudnnDestroyActivationDescriptor(self.relu);
            cudnn::cudnnDestroyConvolutionDescriptor(self.descr);
        }
    }
}

impl UpLayer {
    /// Create a single convolutional layer.
    ///
    /// # Arguments
    /// 
    /// * `handle` - The cuDNN handle
    /// * `n` - The number of images.
    /// * `tensors` - 
    /// 
    unsafe fn new(handle: &cudnn::Handle, n: i32, tensors: &HashMap<String, Tensor>) -> UpLayer {
        let weights = &tensors["01_upsample/weights:0"];
        let mut out = UpLayer {
            input: ptr::null(),
            output: ptr::null(),
            offset: ptr::null(),
            filter: ptr::null(),
            relu: ptr::null(),
            descr: ptr::null(),

            fwd_algo: cudnn::ConvolutionFwdAlgoPerf::new(),
            alpha: weights.scale / (127.0 * 6.0)
        };

        check!(cudnn::cudnnCreateTensorDescriptor(&mut out.input));
        check!(cudnn::cudnnSetTensor4dDescriptor(
            out.input,
            cudnn::TensorFormat::NCHWVECTC,
            cudnn::DataType::Int8x4,
            n, NUM_FEATURES as i32, 19, 19
        ));

        check!(cudnn::cudnnCreateTensorDescriptor(&mut out.output));
        check!(cudnn::cudnnSetTensor4dDescriptor(
            out.output,
            cudnn::TensorFormat::NCHWVECTC,
            cudnn::DataType::Int8x4,
            n, NUM_CHANNELS as i32, 19, 19
        ));

        check!(cudnn::cudnnCreateTensorDescriptor(&mut out.offset));
        check!(cudnn::cudnnSetTensor4dDescriptor(
            out.offset,
            cudnn::TensorFormat::NCHW,
            cudnn::DataType::Float,
            1, NUM_CHANNELS as i32, 1, 1
        ));

        check!(cudnn::cudnnCreateFilterDescriptor(&mut out.filter));
        check!(cudnn::cudnnSetFilter4dDescriptor(
            out.filter,
            cudnn::DataType::Int8x4,
            cudnn::TensorFormat::NCHWVECTC,
            NUM_CHANNELS as i32, NUM_FEATURES as i32, 3, 3
        ));

        check!(cudnn::cudnnCreateActivationDescriptor(&mut out.relu));
        check!(cudnn::cudnnSetActivationDescriptor(
            out.relu,
            cudnn::ActivationMode::Relu,
            cudnn::NanPropagation::NotPropagateNan,
            0.0
        ));

        check!(cudnn::cudnnCreateConvolutionDescriptor(&mut out.descr));
        check!(cudnn::cudnnSetConvolution2dDescriptor(
            out.descr,
            1, 1, 1, 1, 1, 1,
            cudnn::ConvolutionMode::CrossCorrelation,
            cudnn::DataType::Int32
        ));

        #[cfg(feature = "tensor-core")] {
            check!(cudnn::cudnnSetConvolutionMathType(out.descr, cudnn::MathType::TensorOpMath));
        }

        // determine the best algorithm to use for this convolution
        let mut num_fwd_algo = 0;

        check!(cudnn::cudnnFindConvolutionForwardAlgorithm(
            *handle,
            out.input,
            out.filter,
            out.descr,
            out.output,
            1, &mut num_fwd_algo, &mut out.fwd_algo
        ));

        assert!(num_fwd_algo > 0);

        out
    }

    unsafe fn forward<T: InferenceType>(
        &self,
        workspace: &mut Workspace,
        input: &SlotGuard
    ) -> SlotGuard
    {
        check!(cudnn::cudnnSetStream(workspace.handle_dnn, workspace.tower_stream));
        check!(cublas::cublasSetStream_v2(workspace.handle_blas, workspace.tower_stream));

        let device_id = get_current_device();
        let weights = &workspace.tensors["01_upsample/weights:0"];
        let offset = &workspace.tensors["01_upsample/offset:0"];

        offset.copy_to_device(device_id, workspace.tower_stream);
        weights.copy_to_device(device_id, workspace.tower_stream);

        // perform the forward convolution
        let workspace_1 = workspace.slots.get_slot(Slot::Workspace_1, self.fwd_algo.memory);
        let output = workspace.slots.get_slot(Slot::Residual_1, size_of::<T::Tower>() * workspace.batch_size * NUM_CHANNELS * 361);

        check!(cudnn::cudnnConvolutionBiasActivationForward(
            workspace.handle_dnn,
            &self.alpha,
            self.input, **input,
            self.filter, weights.get(device_id),
            self.descr, self.fwd_algo.algo,
            *workspace_1, self.fwd_algo.memory,
            &ZERO,
            self.output, *output,
            self.offset, offset.get(device_id),
            self.relu,
            self.output, *output,
        ));

        output
    }
}

struct ResidualLayer {
    tensor: cudnn::TensorDescriptor,
    offset: cudnn::TensorDescriptor,
    filter: cudnn::FilterDescriptor,
    relu: cudnn::ActivationDescriptor,
    descr: cudnn::ConvolutionDescriptor,
    fwd_algo: cudnn::ConvolutionFwdAlgoPerf,

    count: usize,
    alpha1: f32,
    alpha2: f32
}

impl Drop for ResidualLayer {
    fn drop(&mut self) {
        unsafe {
            cudnn::cudnnDestroyTensorDescriptor(self.tensor);
            cudnn::cudnnDestroyTensorDescriptor(self.offset);
            cudnn::cudnnDestroyFilterDescriptor(self.filter);
            cudnn::cudnnDestroyActivationDescriptor(self.relu);
            cudnn::cudnnDestroyConvolutionDescriptor(self.descr);
        }
    }
}

impl ResidualLayer {
    /// Create a layer that takes the final output of the residual block and
    /// transforms it into a scalar value.
    ///
    /// # Arguments
    /// 
    /// * `handle` - The cuDNN handle
    /// * `n` - The number of images.
    /// * `i` - What number of residual block this is
    /// * `tensors` - 
    /// 
    unsafe fn new(handle: &cudnn::Handle, n: i32, i: usize, tensors: &HashMap<String, Tensor>) -> ResidualLayer {
        let weights_1 = &tensors[&format!("{:02}_residual/weights_1:0", 2 + i)];
        let weights_2 = &tensors[&format!("{:02}_residual/weights_2:0", 2 + i)];
        let mut out = ResidualLayer {
            tensor: ptr::null(),
            offset: ptr::null(),
            filter: ptr::null(),
            relu: ptr::null(),
            descr: ptr::null(),
            fwd_algo: cudnn::ConvolutionFwdAlgoPerf::new(),

            count: i,
            alpha1: weights_1.scale / 127.0,
            alpha2: weights_2.scale / 127.0,
        };

        check!(cudnn::cudnnCreateTensorDescriptor(&mut out.tensor));
        check!(cudnn::cudnnSetTensor4dDescriptor(
            out.tensor,
            cudnn::TensorFormat::NCHWVECTC,
            cudnn::DataType::Int8x4,
            n, NUM_CHANNELS as i32, 19, 19
        ));

        check!(cudnn::cudnnCreateTensorDescriptor(&mut out.offset));
        check!(cudnn::cudnnSetTensor4dDescriptor(
            out.offset,
            cudnn::TensorFormat::NCHW,
            cudnn::DataType::Float,
            1, NUM_CHANNELS as i32, 1, 1
        ));

        check!(cudnn::cudnnCreateFilterDescriptor(&mut out.filter));
        check!(cudnn::cudnnSetFilter4dDescriptor(
            out.filter,
            cudnn::DataType::Int8x4,
            cudnn::TensorFormat::NCHWVECTC,
            NUM_CHANNELS as i32, NUM_CHANNELS as i32, 3, 3
        ));

        check!(cudnn::cudnnCreateActivationDescriptor(&mut out.relu));
        check!(cudnn::cudnnSetActivationDescriptor(
            out.relu,
            cudnn::ActivationMode::Relu,
            cudnn::NanPropagation::NotPropagateNan,
            0.0
        ));

        check!(cudnn::cudnnCreateConvolutionDescriptor(&mut out.descr));
        check!(cudnn::cudnnSetConvolution2dDescriptor(
            out.descr,
            1, 1, 1, 1, 1, 1,
            cudnn::ConvolutionMode::CrossCorrelation,
            cudnn::DataType::Int32
        ));

        #[cfg(feature = "tensor-core")] {
            check!(cudnn::cudnnSetConvolutionMathType(out.descr, cudnn::MathType::TensorOpMath));
        }

        // determine the best algorithm to use for this convolution
        let mut num_fwd_algo = 0;

        check!(cudnn::cudnnFindConvolutionForwardAlgorithm(
            *handle,
            out.tensor,
            out.filter,
            out.descr,
            out.tensor,
            1, &mut num_fwd_algo, &mut out.fwd_algo
        ));

        assert!(num_fwd_algo > 0);

        out
    }

    unsafe fn forward<T: InferenceType>(
        &self,
        workspace: &mut Workspace,
        input: SlotGuard
    ) -> SlotGuard
    {
        check!(cudnn::cudnnSetStream(workspace.handle_dnn, workspace.tower_stream));
        check!(cublas::cublasSetStream_v2(workspace.handle_blas, workspace.tower_stream));

        let device_id = get_current_device();
        let weights_1 = &workspace.tensors[&format!("{:02}_residual/weights_1:0", 2 + self.count)];
        let weights_2 = &workspace.tensors[&format!("{:02}_residual/weights_2:0", 2 + self.count)];
        let offset_1 = &workspace.tensors[&format!("{:02}_residual/offset_1:0", 2 + self.count)];
        let offset_2 = &workspace.tensors[&format!("{:02}_residual/offset_2:0", 2 + self.count)];

        weights_1.copy_to_device(device_id, workspace.tower_stream);
        weights_2.copy_to_device(device_id, workspace.tower_stream);
        offset_1.copy_to_device(device_id, workspace.tower_stream);
        offset_2.copy_to_device(device_id, workspace.tower_stream);

        // perform the forward convolution (1)
        let workspace_r = workspace.slots.get_slot(Slot::Workspace_r, self.fwd_algo.memory);
        let residual_2 = workspace.slots.get_slot(Slot::Residual_2, size_of::<T::Tower>() * workspace.batch_size * NUM_CHANNELS * 361);

        check!(cudnn::cudnnConvolutionBiasActivationForward(
            workspace.handle_dnn,
            &self.alpha1,
            self.tensor, *input,
            self.filter, weights_1.get(device_id),
            self.descr, self.fwd_algo.algo,
            *workspace_r, self.fwd_algo.memory,
            &ZERO,
            self.tensor, *residual_2,
            self.offset, offset_1.get(device_id),
            self.relu,
            self.tensor, *residual_2
        ));

        // perform the forward convolution (2)
        check!(cudnn::cudnnConvolutionBiasActivationForward(
            workspace.handle_dnn,
            &self.alpha2,
            self.tensor, *residual_2,
            self.filter, weights_2.get(device_id),
            self.descr, self.fwd_algo.algo,
            *workspace_r, self.fwd_algo.memory,
            &ONE,
            self.tensor, *input,
            self.offset, offset_2.get(device_id),
            self.relu,
            self.tensor, *input
        ));

        input
    }
}

struct ValueLayer {
    input: cudnn::TensorDescriptor,
    offset: cudnn::TensorDescriptor,
    filter: cudnn::FilterDescriptor,
    relu: cudnn::ActivationDescriptor,
    descr: cudnn::ConvolutionDescriptor,
    fwd_algo: cudnn::ConvolutionFwdAlgoPerf,

    value_1: cudnn::TensorDescriptor,
    value_2: cudnn::TensorDescriptor,
    value_3: cudnn::TensorDescriptor,
    bias_1: cudnn::TensorDescriptor,
    bias_2: cudnn::TensorDescriptor,
    tanh: cudnn::ActivationDescriptor,

    alpha1: f32,
    alpha2: f32
}

impl Drop for ValueLayer {
    fn drop(&mut self) {
        unsafe {
            cudnn::cudnnDestroyTensorDescriptor(self.input);
            cudnn::cudnnDestroyTensorDescriptor(self.offset);
            cudnn::cudnnDestroyFilterDescriptor(self.filter);
            cudnn::cudnnDestroyActivationDescriptor(self.relu);
            cudnn::cudnnDestroyConvolutionDescriptor(self.descr);

            cudnn::cudnnDestroyTensorDescriptor(self.value_1);
            cudnn::cudnnDestroyTensorDescriptor(self.value_2);
            cudnn::cudnnDestroyTensorDescriptor(self.value_3);
            cudnn::cudnnDestroyTensorDescriptor(self.bias_1);
            cudnn::cudnnDestroyTensorDescriptor(self.bias_2);
            cudnn::cudnnDestroyActivationDescriptor(self.tanh);
        }
    }
}

impl ValueLayer {
    /// Create a layer that takes the final output of the residual block and
    /// transforms it into a scalar value.
    ///
    /// # Arguments
    /// 
    /// * `handle` - The cuDNN handle
    /// * `n` - The number of images.
    /// * `tensors` - 
    /// 
    unsafe fn new(handle: &cudnn::Handle, n: i32, tensors: &HashMap<String, Tensor>) -> ValueLayer {
        let weights_1 = &tensors[&format!("{:02}v_value/downsample:0", 2 + NUM_LAYERS)];
        let mut out = ValueLayer {
            input: ptr::null(),
            offset: ptr::null(),
            filter: ptr::null(),
            relu: ptr::null(),
            descr: ptr::null(),
            fwd_algo: cudnn::ConvolutionFwdAlgoPerf::new(),

            value_1: ptr::null(),
            value_2: ptr::null(),
            value_3: ptr::null(),
            bias_1: ptr::null(),
            bias_2: ptr::null(),
            tanh: ptr::null(),

            alpha1: weights_1.scale / 127.0,
            alpha2: 6.0 / 127.0
        };

        check!(cudnn::cudnnCreateTensorDescriptor(&mut out.input));
        check!(cudnn::cudnnSetTensor4dDescriptor(
            out.input,
            cudnn::TensorFormat::NCHWVECTC,
            cudnn::DataType::Int8x4,
            n, NUM_CHANNELS as i32, 19, 19
        ));

        check!(cudnn::cudnnCreateTensorDescriptor(&mut out.value_1));
        check!(cudnn::cudnnSetTensor4dDescriptor(
            out.value_1,
            cudnn::TensorFormat::NCHW,
            cudnn::DataType::Float,
            n, 1, 19, 19
        ));

        check!(cudnn::cudnnCreateTensorDescriptor(&mut out.value_2));
        check!(cudnn::cudnnSetTensor4dDescriptor(
            out.value_2,
            cudnn::TensorFormat::NCHW,
            cudnn::DataType::Float,
            n, 256, 1, 1
        ));

        check!(cudnn::cudnnCreateTensorDescriptor(&mut out.value_3));
        check!(cudnn::cudnnSetTensor4dDescriptor(
            out.value_3,
            cudnn::TensorFormat::NCHW,
            cudnn::DataType::Float,
            n, 1, 1, 1
        ));

        check!(cudnn::cudnnCreateTensorDescriptor(&mut out.offset));
        check!(cudnn::cudnnSetTensor4dDescriptor(
            out.offset,
            cudnn::TensorFormat::NCHW,
            cudnn::DataType::Float,
            1, 1, 1, 1
        ));

        check!(cudnn::cudnnCreateTensorDescriptor(&mut out.bias_1));
        check!(cudnn::cudnnSetTensor4dDescriptor(
            out.bias_1,
            cudnn::TensorFormat::NCHW,
            cudnn::DataType::Float,
            1, 256, 1, 1
        ));

        check!(cudnn::cudnnCreateTensorDescriptor(&mut out.bias_2));
        check!(cudnn::cudnnSetTensor4dDescriptor(
            out.bias_2,
            cudnn::TensorFormat::NCHW,
            cudnn::DataType::Float,
            1, 1, 1, 1
        ));

        check!(cudnn::cudnnCreateFilterDescriptor(&mut out.filter));
        check!(cudnn::cudnnSetFilter4dDescriptor(
            out.filter,
            cudnn::DataType::Int8x4,
            cudnn::TensorFormat::NCHWVECTC,
            1, NUM_CHANNELS as i32, 1, 1
        ));

        check!(cudnn::cudnnCreateActivationDescriptor(&mut out.relu));
        check!(cudnn::cudnnSetActivationDescriptor(
            out.relu,
            cudnn::ActivationMode::Relu,
            cudnn::NanPropagation::NotPropagateNan,
            0.0
        ));

        check!(cudnn::cudnnCreateActivationDescriptor(&mut out.tanh));
        check!(cudnn::cudnnSetActivationDescriptor(
            out.tanh,
            cudnn::ActivationMode::Tanh,
            cudnn::NanPropagation::NotPropagateNan,
            0.0
        ));

        check!(cudnn::cudnnCreateConvolutionDescriptor(&mut out.descr));
        check!(cudnn::cudnnSetConvolution2dDescriptor(
            out.descr,
            0, 0, 1, 1, 1, 1,
            cudnn::ConvolutionMode::CrossCorrelation,
            cudnn::DataType::Int32
        ));

        #[cfg(feature = "tensor-core")] {
            check!(cudnn::cudnnSetConvolutionMathType(out.descr, cudnn::MathType::TensorOpMath));
        }

        // determine the best algorithm to use for this convolution
        let mut num_fwd_algo = 0;

        check!(cudnn::cudnnFindConvolutionForwardAlgorithm(
            *handle,
            out.input,
            out.filter,
            out.descr,
            out.value_1,
            1, &mut num_fwd_algo, &mut out.fwd_algo
        ));

        assert!(num_fwd_algo > 0);

        out
    }

    unsafe fn forward<T: InferenceType>(
        &self,
        workspace: &mut Workspace,
        output_set: &OutputSet,
        output_map: &mut OutputMap<Vec<f32>>,
        input: &SlotGuard
    ) -> SlotGuard
    {
        check!(cudnn::cudnnSetStream(workspace.handle_dnn, workspace.value_stream));
        check!(cublas::cublasSetStream_v2(workspace.handle_blas, workspace.value_stream));

        let device_id = get_current_device();
        let weights_1 = &workspace.tensors[&format!("{:02}v_value/downsample:0", 2 + NUM_LAYERS)];
        let weights_2 = &workspace.tensors[&format!("{:02}v_value/weights_1:0", 2 + NUM_LAYERS)];
        let weights_3 = &workspace.tensors[&format!("{:02}v_value/weights_2:0", 2 + NUM_LAYERS)];
        let offset_1 = &workspace.tensors[&format!("{:02}v_value/offset:0", 2 + NUM_LAYERS)];
        let offset_2 = &workspace.tensors[&format!("{:02}v_value/bias_1:0", 2 + NUM_LAYERS)];
        let offset_3 = &workspace.tensors[&format!("{:02}v_value/bias_2:0", 2 + NUM_LAYERS)];

        weights_1.copy_to_device(device_id, workspace.value_stream);
        weights_2.copy_to_device(device_id, workspace.value_stream);
        weights_3.copy_to_device(device_id, workspace.value_stream);
        offset_1.copy_to_device(device_id, workspace.value_stream);
        offset_2.copy_to_device(device_id, workspace.value_stream);
        offset_3.copy_to_device(device_id, workspace.value_stream);

        // perform the forward convolution
        let workspace_v = workspace.slots.get_slot(Slot::Workspace_v, self.fwd_algo.memory);
        let value_1 = workspace.slots.get_slot(Slot::Value_1, size_of::<T::Output>() * workspace.batch_size * 361);

        check!(cudnn::cudnnConvolutionBiasActivationForward(
            workspace.handle_dnn,
            &self.alpha1,
            self.input, **input,
            self.filter, weights_1.get(device_id),
            self.descr, self.fwd_algo.algo,
            *workspace_v, self.fwd_algo.memory,
            &ZERO,
            self.value_1, *value_1,
            self.offset, offset_1.get(device_id),
            self.relu,
            self.value_1, *value_1
        ));

        output_set.contains(Output::ValueDown).map(|key| { output_map.put(key, load_to_host::<T::Output>(*value_1, workspace.batch_size * 361, workspace.value_stream)) });

        // perform the feed-forward linear layer (relu)
        let value_2 = workspace.slots.get_slot(Slot::Value_2, size_of::<T::Output>() * workspace.batch_size * 256);
        let value_3 = workspace.slots.get_slot(Slot::Value_3, size_of::<T::Output>() * workspace.batch_size * 1);

        check!(cublas::cublasGemmEx(
            workspace.handle_blas,
            cublas::Operation::N,
            cublas::Operation::N,
            256, workspace.batch_size as i32, 361,  // output, batch_size, input
            &self.alpha2 as *const f32 as *const c_void,
            weights_2.get(device_id), cuda::DataType::R32F, 256,  // input_2
            *value_1, cuda::DataType::R32F, 361,  // input_1
            &ZERO as *const f32 as *const c_void,
            *value_2, cuda::DataType::R32F, 256,  // output
            cuda::DataType::R32F, cublas::GemmAlgo::Dfalt
        ));

        check!(cudnn::cudnnAddTensor(
            workspace.handle_dnn,
            &ONE, self.bias_1, offset_2.get(device_id),
            &ONE, self.value_2, *value_2
        ));

        check!(cudnn::cudnnActivationForward(
            workspace.handle_dnn,
            self.relu,
            &ONE, self.value_2, *value_2,  // input
            &ZERO, self.value_2, *value_2,  // output
        ));

        // perform the feed-forward linear layer (tanh)
        check!(cublas::cublasGemmEx(
            workspace.handle_blas,
            cublas::Operation::N,
            cublas::Operation::N,
            1, workspace.batch_size as i32, 256,  // output, batch_size, input
            &ONE as *const f32 as *const c_void,
            weights_3.get(device_id), cuda::DataType::R32F, 1,  // input_2
            *value_2, cuda::DataType::R32F, 256,  // input_1
            &ZERO as *const f32 as *const c_void,
            *value_3, cuda::DataType::R32F, 1,  // output
            cuda::DataType::R32F, cublas::GemmAlgo::Dfalt
        ));

        check!(cudnn::cudnnAddTensor(
            workspace.handle_dnn,
            &ONE, self.bias_2, offset_3.get(device_id),
            &ONE, self.value_3, *value_3
        ));

        check!(cudnn::cudnnActivationForward(
            workspace.handle_dnn,
            self.tanh,
            &ONE, self.value_3, *value_3,  // input
            &ZERO, self.value_3, *value_3,  // output
        ));

        value_3
    }
}

struct PolicyLayer {
    input: cudnn::TensorDescriptor,
    offset: cudnn::TensorDescriptor,
    filter: cudnn::FilterDescriptor,
    relu: cudnn::ActivationDescriptor,
    descr: cudnn::ConvolutionDescriptor,
    fwd_algo: cudnn::ConvolutionFwdAlgoPerf,

    bias: cudnn::TensorDescriptor,

    policy_1: cudnn::TensorDescriptor,
    policy_2: cudnn::TensorDescriptor,

    alpha1: f32,
    alpha2: f32
}

impl Drop for PolicyLayer {
    fn drop(&mut self) {
        unsafe {
            cudnn::cudnnDestroyTensorDescriptor(self.input);
            cudnn::cudnnDestroyTensorDescriptor(self.offset);
            cudnn::cudnnDestroyFilterDescriptor(self.filter);
            cudnn::cudnnDestroyActivationDescriptor(self.relu);
            cudnn::cudnnDestroyConvolutionDescriptor(self.descr);

            cudnn::cudnnDestroyTensorDescriptor(self.bias);

            cudnn::cudnnDestroyTensorDescriptor(self.policy_1);
            cudnn::cudnnDestroyTensorDescriptor(self.policy_2);
        }
    }
}

impl PolicyLayer {
    /// Create a layer that takes the final output of the residual block and
    /// transforms it into a policy vector.
    ///
    /// # Arguments
    /// 
    /// * `handle` - The cuDNN handle
    /// * `n` - The number of images.
    /// * `tensors` - 
    /// 
    unsafe fn new(handle: &cudnn::Handle, n: i32, tensors: &HashMap<String, Tensor>) -> PolicyLayer {
        let weights_1 = &tensors[&format!("{:02}p_policy/downsample:0", 2 + NUM_LAYERS)];
        let mut out = PolicyLayer {
            input: ptr::null(),
            offset: ptr::null(),
            filter: ptr::null(),
            relu: ptr::null(),
            descr: ptr::null(),
            fwd_algo: cudnn::ConvolutionFwdAlgoPerf::new(),

            bias: ptr::null(),

            policy_1: ptr::null(),
            policy_2: ptr::null(),

            alpha1: weights_1.scale / 127.0,
            alpha2: 6.0 / 127.0
        };

        check!(cudnn::cudnnCreateTensorDescriptor(&mut out.input));
        check!(cudnn::cudnnSetTensor4dDescriptor(
            out.input,
            cudnn::TensorFormat::NCHWVECTC,
            cudnn::DataType::Int8x4,
            n, NUM_CHANNELS as i32, 19, 19
        ));

        check!(cudnn::cudnnCreateTensorDescriptor(&mut out.policy_1));
        check!(cudnn::cudnnSetTensor4dDescriptor(
            out.policy_1,
            cudnn::TensorFormat::NCHW,
            cudnn::DataType::Float,
            n, 2, 19, 19
        ));

        check!(cudnn::cudnnCreateTensorDescriptor(&mut out.policy_2));
        check!(cudnn::cudnnSetTensor4dDescriptor(
            out.policy_2,
            cudnn::TensorFormat::NCHW,
            cudnn::DataType::Float,
            n, 362, 1, 1
        ));

        check!(cudnn::cudnnCreateTensorDescriptor(&mut out.offset));
        check!(cudnn::cudnnSetTensor4dDescriptor(
            out.offset,
            cudnn::TensorFormat::NCHW,
            cudnn::DataType::Float,
            1, 2, 1, 1
        ));

        check!(cudnn::cudnnCreateTensorDescriptor(&mut out.bias));
        check!(cudnn::cudnnSetTensor4dDescriptor(
            out.bias,
            cudnn::TensorFormat::NCHW,
            cudnn::DataType::Float,
            1, 362, 1, 1
        ));

        check!(cudnn::cudnnCreateFilterDescriptor(&mut out.filter));
        check!(cudnn::cudnnSetFilter4dDescriptor(
            out.filter,
            cudnn::DataType::Int8x4,
            cudnn::TensorFormat::NCHWVECTC,
            2, NUM_CHANNELS as i32, 1, 1
        ));

        check!(cudnn::cudnnCreateActivationDescriptor(&mut out.relu));
        check!(cudnn::cudnnSetActivationDescriptor(
            out.relu,
            cudnn::ActivationMode::Relu,
            cudnn::NanPropagation::NotPropagateNan,
            0.0
        ));

        check!(cudnn::cudnnCreateConvolutionDescriptor(&mut out.descr));
        check!(cudnn::cudnnSetConvolution2dDescriptor(
            out.descr,
            0, 0, 1, 1, 1, 1,
            cudnn::ConvolutionMode::CrossCorrelation,
            cudnn::DataType::Int32
        ));

        #[cfg(feature = "tensor-core")] {
            check!(cudnn::cudnnSetConvolutionMathType(out.descr, cudnn::MathType::TensorOpMath));
        }

        // determine the best algorithm to use for this convolution
        let mut num_fwd_algo = 0;

        check!(cudnn::cudnnFindConvolutionForwardAlgorithm(
            *handle,
            out.input,
            out.filter,
            out.descr,
            out.policy_1,
            1, &mut num_fwd_algo, &mut out.fwd_algo
        ));

        assert!(num_fwd_algo > 0);

        out
    }

    unsafe fn forward<T: InferenceType>(
        &self,
        workspace: &mut Workspace,
        output_set: &OutputSet,
        output_map: &mut OutputMap<Vec<f32>>,
        input: &SlotGuard
    ) -> SlotGuard
    {
        check!(cudnn::cudnnSetStream(workspace.handle_dnn, workspace.policy_stream));
        check!(cublas::cublasSetStream_v2(workspace.handle_blas, workspace.policy_stream));

        let device_id = get_current_device();
        let weights_1 = &workspace.tensors[&format!("{:02}p_policy/downsample:0", 2 + NUM_LAYERS)];
        let weights_2 = &workspace.tensors[&format!("{:02}p_policy/weights:0", 2 + NUM_LAYERS)];
        let offset_1 = &workspace.tensors[&format!("{:02}p_policy/offset:0", 2 + NUM_LAYERS)];
        let offset_2 = &workspace.tensors[&format!("{:02}p_policy/bias:0", 2 + NUM_LAYERS)];

        offset_1.copy_to_device(device_id, workspace.policy_stream);
        offset_2.copy_to_device(device_id, workspace.policy_stream);
        weights_1.copy_to_device(device_id, workspace.policy_stream);
        weights_2.copy_to_device(device_id, workspace.policy_stream);

        // perform the forward convolution
        let workspace_p = workspace.slots.get_slot(Slot::Workspace_p, self.fwd_algo.memory);
        let policy_1 = workspace.slots.get_slot(Slot::Policy_1, size_of::<T::Output>() * workspace.batch_size * 2 * 361);

        check!(cudnn::cudnnConvolutionBiasActivationForward(
            workspace.handle_dnn,
            &self.alpha1,
            self.input, **input,
            self.filter, weights_1.get(device_id),
            self.descr, self.fwd_algo.algo,
            *workspace_p, self.fwd_algo.memory,
            &ZERO,
            self.policy_1, *policy_1,
            self.offset, offset_1.get(device_id),
            self.relu,
            self.policy_1, *policy_1
        ));

        output_set.contains(Output::PolicyDown).map(|key| { output_map.put(key, load_to_host::<T::Output>(*policy_1, workspace.batch_size * 2 * 361, workspace.policy_stream)) });

        // perform the feed-forward linear layers
        let policy_2 = workspace.slots.get_slot(Slot::Policy_2, size_of::<T::Output>() * workspace.batch_size * 362);
        let policy_3 = workspace.slots.get_slot(Slot::Policy_3, size_of::<T::Output>() * workspace.batch_size * 362);

        check!(cublas::cublasGemmEx(
            workspace.handle_blas,
            cublas::Operation::N,
            cublas::Operation::N,
            362, workspace.batch_size as i32, 722,  // output, batch_size, input
            &self.alpha2 as *const f32 as *const c_void,
            weights_2.get(device_id), cuda::DataType::R32F, 362,  // input_2
            *policy_1, cuda::DataType::R32F, 722,  // input_1
            &ZERO as *const f32 as *const c_void,
            *policy_2, cuda::DataType::R32F, 362,  // output
            cuda::DataType::R32F, cublas::GemmAlgo::Dfalt
        ));

        check!(cudnn::cudnnAddTensor(
            workspace.handle_dnn,
            &ONE, self.bias, offset_2.get(device_id),
            &ONE, self.policy_2, *policy_2
        ));

        check!(cudnn::cudnnActivationForward(
            workspace.handle_dnn,
            self.relu,
            &ONE, self.policy_2, *policy_2,  // input
            &ZERO, self.policy_2, *policy_2,  // output
        ));

        // softmax activation
        check!(cudnn::cudnnSoftmaxForward(
            workspace.handle_dnn,
            cudnn::SoftmaxAlgorithm::Accurate,
            cudnn::SoftmaxMode::Channel,
            &ONE, self.policy_2, *policy_2,  // input
            &ZERO, self.policy_2, *policy_3,  // output
        ));

        policy_3
    }
}

/// Returns the value and policy tensors obtained from a forward pass
/// through the neural network.
///
/// # Arguments
///
/// * `workspace` - the workspace for the current thread
/// * `features` - the input features
/// * `outputs` - the outputs to copy to host memory
///
pub fn forward<T: InferenceType>(
    workspace: &mut Workspace,
    features: &[T],
    outputs: OutputSet
) -> OutputMap<Vec<f32>>
{
    debug_assert!(features.len() % FEATURE_SIZE == 0);
    debug_assert!(features.len() / FEATURE_SIZE == workspace.batch_size);

    let mut map = OutputMap::new();

    unsafe {
        check!(cudnn::cudnnSetStream(workspace.handle_dnn, workspace.tower_stream));

        // copy all of the input features into a temporary workspace
        let input = workspace.slots.get_slot(Slot::Input, size_of::<T>() * features.len());
        let image_size = NUM_CHANNELS * 361;

        check!(cuda::cudaMemcpyAsync(
            *input,
            features.as_ptr() as *const c_void,
            size_of::<T>() * features.len(),
            cuda::MemcpyKind::HostToDevice,
            workspace.tower_stream
        ));

        // Upsample 32 -> 128 channels
        let mut residual_1 = workspace.c_up.clone().forward::<T>(workspace, &input);

        outputs.contains(Output::Upsample).map(|key| { map.put(key, load_to_host::<T::Tower>(*residual_1, workspace.batch_size * image_size, workspace.tower_stream)) });

        // residual blocks
        for i in 0..NUM_LAYERS {
            let residual = workspace.c_residual[i].clone();
            let output = ::std::mem::transmute(Output::Residual_00 as u8 + i as u8);

            residual_1 = residual.forward::<T>(workspace, residual_1);

            outputs.contains(output).map(|key| { map.put(key, load_to_host::<T::Output>(*residual_1, workspace.batch_size * NUM_CHANNELS * 361, workspace.tower_stream)) });
        }

        check!(cuda::cudaEventRecord(workspace.tower_finished, workspace.tower_stream));
        check!(cuda::cudaStreamWaitEvent(workspace.value_stream, workspace.tower_finished, 0));
        check!(cuda::cudaStreamWaitEvent(workspace.policy_stream, workspace.tower_finished, 0));

        // run the value and policy head, then wait for them to finish (if
        // they are requested)
        let value = workspace.c_value.clone().forward::<T>(workspace, &outputs, &mut map, &residual_1);
        let policy = workspace.c_policy.clone().forward::<T>(workspace, &outputs, &mut map, &residual_1);

        outputs.contains(Output::Value).map(|key| { map.put(key, load_to_host::<T::Output>(*value, workspace.batch_size, workspace.value_stream)) });
        outputs.contains(Output::Policy).map(|key| { map.put(key, load_to_host::<T::Output>(*policy, workspace.batch_size * 362, workspace.policy_stream)) });
    }

    // pretty-print the tensor to stderr if logging is turned on
    if cfg!(feature = "trace-cuda") {
        for name in outputs.iter() {
            let mut dbg = String::new();
            let host = map.get(name);

            for (i, value) in host.iter().enumerate() {
                if i > 0 { dbg += ", "; }

                dbg += &format!("{:.4}", value);
            }

            eprintln!("{:?} = [{}]", name, dbg);
        }
    }

    map
}

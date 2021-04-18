# Copyright (c) 2019 Karl Sundequist Blomdahl <karl.sundequist.blomdahl@gmail.com>
#
# Permission is hereby granted, free of charge, to any person obtaining a copy
# of this software and associated documentation files (the "Software"), to deal
# in the Software without restriction, including without limitation the rights
# to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
# copies of the Software, and to permit persons to whom the Software is
# furnished to do so, subject to the following conditions:
#
# The above copyright notice and this permission notice shall be included in all
# copies or substantial portions of the Software.
#
# THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
# IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
# FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
# AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
# LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
# OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
# SOFTWARE.

import tensorflow as tf

from .moving_average import moving_average
from .orthogonal_initializer import orthogonal_initializer
from . import conv2d, normalize_getting, l2_regularizer
from ..hooks.dump import DUMP_OPS

THREE = 3.09023

def relu3(x):
    return tf.clip_by_value(x, 0.0, THREE)


def batch_norm_conv2d(x, op_name, shape, mode, params, is_recomputing=False):
    weights = tf.compat.v1.get_variable(op_name, shape, tf.float32, orthogonal_initializer(), custom_getter=normalize_getting, regularizer=l2_regularizer, use_resource=True)

    return batch_norm(conv2d(x, weights), weights, op_name, mode, params, is_recomputing=is_recomputing)


def batch_norm(x, weights, op_name, mode, params, is_recomputing=False):
    """ Batch normalization layer. """
    num_channels = weights.shape[3]
    ones_op = tf.compat.v1.ones_initializer()
    zeros_op = tf.compat.v1.zeros_initializer()

    with tf.compat.v1.variable_scope(op_name) as name_scope:
        scale = tf.compat.v1.get_variable('scale', (num_channels,), tf.float32, ones_op, trainable=False, use_resource=True)
        mean = tf.compat.v1.get_variable('mean', (num_channels,), tf.float32, zeros_op, trainable=False, use_resource=True)
        variance = tf.compat.v1.get_variable('variance', (num_channels,), tf.float32, ones_op, trainable=False, use_resource=True)
        offset = tf.compat.v1.get_variable('offset', (num_channels,), tf.float32, zeros_op, trainable=True, use_resource=True)

    if not is_recomputing:
        # fold the batch normalization into the convolutional weights and one
        # additional bias term. By scaling the weights and the mean by the
        # term `scale / sqrt(variance + 0.001)`.
        #
        # Also multiply the mean by -1 since the bias term uses addition, while
        # batch normalization assumes subtraction.
        #
        # The weights are scaled using broadcasting, where all input weights for
        # a given output feature are scaled by that features term.
        #
        std_ = tf.sqrt(variance + 0.001)
        offset_ = offset - mean / std_
        weights_ = tf.multiply(
            weights,
            tf.reshape(scale / std_, (1, 1, 1, num_channels))
        )

        # fix the weights so that they appear in the _correct_ order according
        # to cuDNN (for NHWC):
        #
        # tensorflow: [h, w, in, out]
        # cudnn:      [out, h, w, in]
        weights_ = tf.transpose(a=weights_, perm=[3, 0, 1, 2])

        # calulate the moving average of the weights and the offset.
        weights_ma = moving_average(weights_, f'{op_name}/moving_avg', mode)
        offset_ma = moving_average(offset_, f'{op_name}/offset/moving_avg', mode)

        # quantize the weights to [-127, +127] and offset to the same range (but
        # as a floating point number as required by cuDNN).
        #
        # We do the funny `tf.stack` and flatten because `tf.reduce_max` does
        # not want to behave with the `moving_average` output tensor.
        weights_max = tf.reduce_max(input_tensor=tf.reshape(tf.stack([weights_ma, -weights_ma]), [-1]))
        weights_q, weights_qmin, weights_qmax = tf.quantization.quantize(
            weights_ma,
            -weights_max,
            weights_max,
            tf.qint8,
            'SCALED',
            'HALF_AWAY_FROM_ZERO'
        )

        step_size = (2 * THREE) / 255.0

        tf.compat.v1.add_to_collection(DUMP_OPS, [offset.name, offset_ma / step_size, 'f4', tf.constant(127 * step_size)])
        tf.compat.v1.add_to_collection(DUMP_OPS, [f'{name_scope.name}:0', weights_q, 'i1', tf.math.reduce_max(input_tensor=tf.stack([weights_qmin, -weights_qmin, weights_qmax, -weights_qmax]))])

    def _forward(x):
        """ Returns the result of the forward inference pass on `x` """
        if mode == tf.estimator.ModeKeys.TRAIN:
            y, b_mean, b_variance = tf.compat.v1.nn.fused_batch_norm(
                x,
                scale,
                offset,
                None,
                None,
                data_format='NHWC',
                is_training=True
            )

            if not is_recomputing:
                with tf.device(None):
                    update_mean_op = tf.compat.v1.assign_sub(mean, 0.01 * (mean - b_mean), use_locking=True)
                    update_variance_op = tf.compat.v1.assign_sub(variance, 0.01 * (variance - b_variance), use_locking=True)

                    tf.compat.v1.add_to_collection(tf.compat.v1.GraphKeys.UPDATE_OPS, update_mean_op)
                    tf.compat.v1.add_to_collection(tf.compat.v1.GraphKeys.UPDATE_OPS, update_variance_op)
        else:
            y, _, _ = tf.compat.v1.nn.fused_batch_norm(
                x,
                scale,
                offset,
                mean,
                variance,
                data_format='NHWC',
                is_training=False
            )

        return y

    return _forward(x)

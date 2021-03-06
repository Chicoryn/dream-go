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
from . import cast_to_compute_type, normalize_getting
from ..hooks.dump import DUMP_OPS

def batch_norm_conv2d(x, op_name, shape, mode, params, is_recomputing=False):
    weights = tf.compat.v1.get_variable(op_name, shape, tf.float32, orthogonal_initializer(), custom_getter=normalize_getting, collections=[tf.compat.v1.GraphKeys.GLOBAL_VARIABLES, tf.compat.v1.GraphKeys.WEIGHTS], use_resource=True)
    y = tf.nn.conv2d(input=x, filters=cast_to_compute_type(weights), strides=(1, 1, 1, 1), padding='SAME', data_format='NHWC')

    return batch_norm(y, weights, op_name, mode, params, is_recomputing=is_recomputing)


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

    if not is_recomputing and 'no_dump' not in params:
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
        # cudnn:      [out, in, h, w]
        weights_ = tf.transpose(a=weights_, perm=[3, 0, 1, 2])

        # calulate the moving average of the weights and the offset.
        tf.compat.v1.add_to_collection(DUMP_OPS, [offset.name, moving_average(offset_, f'{op_name}/offset/moving_avg', mode), 'f2'])
        tf.compat.v1.add_to_collection(DUMP_OPS, [f'{name_scope.name}:0', moving_average(weights_, f'{op_name}/moving_avg', mode), 'f2'])

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

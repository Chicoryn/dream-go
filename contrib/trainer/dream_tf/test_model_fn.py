# Copyright (c) 2020 Karl Sundequist Blomdahl <karl.sundequist.blomdahl@gmail.com>
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
import unittest

from .model_fn import model_fn
from .layers import NUM_FEATURES

class ModelFnTest(unittest.TestCase):
    def setUp(self):
        self.batch_size = 16
        self.features = tf.compat.v1.placeholder(tf.float16, [self.batch_size, 19, 19, NUM_FEATURES])
        self.labels = {
            'lz_features': tf.compat.v1.placeholder(tf.float16, [self.batch_size, 19, 19, 18]),
            'value': tf.compat.v1.placeholder(tf.float32, [self.batch_size, 1]),
            'policy': tf.compat.v1.placeholder(tf.float32, [self.batch_size, 362]),
            'next_policy': tf.compat.v1.placeholder(tf.float32, [self.batch_size, 362]),
            'boost': tf.compat.v1.placeholder(tf.float32, [self.batch_size, 1]),
            'ownership': tf.compat.v1.placeholder(tf.float32, [self.batch_size, 361]),
            'has_ownership': tf.compat.v1.placeholder(tf.float32, [self.batch_size, 1]),
        }
        self.spec = model_fn(self.features, self.labels, self.mode, self.params)

    @property
    def mode(self):
        return tf.estimator.ModeKeys.TRAIN

    @property
    def params(self):
        return {
            'num_blocks': 6,
            'num_channels': 64,
            'learning_rate': 1e-4,
            'num_samples': 8,
            'model_name': 'test'
        }

    def tearDown(self):
        tf.compat.v1.reset_default_graph()

    def test_mode(self):
        self.assertEqual(self.spec.mode, self.mode)

    def test_shape(self):
        self.assertEqual(self.spec.loss.shape, [])

    def test_data_type(self):
        self.assertEqual(self.spec.loss.dtype, tf.float32)


class LzModelFnTest(ModelFnTest, unittest.TestCase):
    @property
    def params(self):
        return {
            'num_blocks': 6,
            'num_channels': 64,
            'learning_rate': 1e-4,
            'num_samples': 8,
            'lz_weights': 'fixtures/d645af9.gz',
            'model_name': 'test'
        }


if __name__ == '__main__':
    unittest.main()

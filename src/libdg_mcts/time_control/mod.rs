// Copyright 2019 Karl Sundequist Blomdahl <karl.sundequist.blomdahl@gmail.com>
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

mod byo_yomi;
mod rollout_limit;

pub use self::byo_yomi::*;
pub use self::rollout_limit::*;

use tree;

pub enum TimeStrategyResult {
    NotExpired(usize),
    NotExtended,
    Expired,
    Extended
}

pub trait TimeStrategy {
    /// Checking if this time period has expired, and if so calls `predicate` to
    /// determine whether we should attempt to extend it further.
    ///
    /// # Arguments
    ///
    /// * `root` - the root of the search tree.
    ///
    fn try_extend(&self, root: &tree::Node) -> TimeStrategyResult;
}

/// Returns the minimum number of playouts that are necessary for the second
/// most visited child to become the most visited child.
///
/// # Arguments
///
/// * `root` - the tree to get the lower bound for
///
fn min_promote_rollouts(root: &tree::Node) -> usize {
    let top_1 = root.children.argmax_count();

    // find the most visited child that is **not** `top_1`.
    let mut top_2 = if top_1 == 0 { 1 } else { 0 };

    for i in root.children.nonzero() {
        let count_i = root.children.with(i, |child| child.count(), root.initial_value);

        if i != top_1 && count_i > root.children.with(top_2, |child| child.count(), root.initial_value) {
            top_2 = i;
        }
    }

    let count_1 = root.children.with(top_1, |child| child.count(), root.initial_value);
    let count_2 = root.children.with(top_2, |child| child.count(), root.initial_value);

    if count_1 > count_2 {
        (count_1 - count_2) as usize
    } else {
        0  // ignore the race condition
    }
}

/// Implements a time control scheme based on the `UNST-N` and `EARLY-C`
/// strategy as suggested by _Hendrik Baier_ and _Mark H.M. Winands_ [1].
///
/// * `UNST-N` extends the search until the most visited also has the highest
///   win rate.
/// * `EARLY-C` terminate the search early if the second most visited node
///   cannot catch up to the most visited node in the remaining time.
///
/// [1] _Hendrik Baier_ and _Mark H.M. Winands_, "Time Management for
///     Monte-Carlo Tree Search in Go", https://pdfs.semanticscholar.org/a2e6/299fd3c8ab17e3a1a783d518688b55bb2363.pdf
///
pub fn is_done(root: &tree::Node, ticket: &Box<dyn TimeStrategy + Sync>) -> bool {
    if root.total_count == 0 {
        false
    } else {
        match ticket.try_extend(root) {
            TimeStrategyResult::NotExpired(remaining) => {
                let min_promote = min_promote_rollouts(root);

                min_promote > remaining
            },
            TimeStrategyResult::Extended => false,
            _ => true
        }
    }
}

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

mod dirichlet;
mod global_cache;
pub mod predict;
mod spin;
pub mod tree;
pub mod time_control;

use ordered_float::OrderedFloat;
use rand::{thread_rng, Rng};
use std::cell::UnsafeCell;
use std::fmt;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use time;

use go::sgf::*;
use go::{symmetry, Board, Color, CHW_VECT_C, Features, Score};
use mcts::time_control::{TimeStrategy, RolloutLimit};
use mcts::predict::{PredictService, PredictGuard, PredictRequest};
use nn::Network;
use util::b85;
use util::config;
use util::min;

pub enum GameResult {
    Resign(String, Board, Color, f32),
    Ended(String, Board)
}

impl fmt::Display for GameResult {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        let now = time::now_utc();
        let iso8601 = time::strftime("%Y-%m-%dT%H:%M:%S%z", &now).unwrap();

        match *self {
            GameResult::Resign(ref sgf, _, winner, _) => {
                write!(fmt, "(;GM[1]FF[4]DT[{}]SZ[19]RU[Chinese]KM[7.5]RE[{}+Resign]{})", iso8601, winner, sgf)
            },
            GameResult::Ended(ref sgf, ref board) => {
                let (black, white) = board.get_score();
                let black = black as f32;
                let white = white as f32 + 7.5;
                let winner = {
                    if black > white {
                        format!("B+{:.1}", black - white)
                    } else if white > black {
                        format!("W+{:.1}", white - black)
                    } else {
                        format!("0")
                    }
                };

                write!(fmt, "(;GM[1]FF[4]DT[{}]SZ[19]RU[Chinese]KM[7.5]RE[{}]{})", iso8601, winner, sgf)
            }
        }
    }
}

/// Performs a forward pass through the neural network for the given board
/// position using a random symmetry to increase entropy.
/// 
/// # Arguments
/// 
/// * `workspace` - the workspace to use during the forward pass
/// * `board` - the board position
/// * `color` - the current player
/// 
fn forward(server: &PredictGuard, board: &Board, color: Color) -> Option<(f32, Box<[f32]>)> {
    lazy_static! {
        static ref SYMM: Vec<symmetry::Transform> = vec! [
            symmetry::Transform::Identity,
            symmetry::Transform::FlipLR,
            symmetry::Transform::FlipUD,
            symmetry::Transform::Transpose,
            symmetry::Transform::TransposeAnti,
            symmetry::Transform::Rot90,
            symmetry::Transform::Rot180,
            symmetry::Transform::Rot270,
        ];
    }

    global_cache::get_or_insert(board, color, || {
        // pick a random transformation to apply to the features. This is done
        // to increase the entropy of the game slightly and to ensure the engine
        // learns the game is symmetric (which should help generalize)
        let t = *thread_rng().choose(&SYMM).unwrap();

        // run a forward pass through the network using this transformation
        // and when we are done undo it using the opposite.
        let response = server.send(PredictRequest::Ask(board.get_features::<CHW_VECT_C>(color, t)));
        let (value, original_policy) = if let Some(x) = response {
            x.unwrap()
        } else {
            return None;
        };

        // copy the policy and replace any invalid moves in the suggested policy
        // with -Inf, while keeping the pass move (361) untouched so that there
        // is always at least one valid move.
        let mut policy = vec! [0.0f32; 362];
        policy[361] = original_policy[361];  // copy `pass` move

        for i in 0..361 {
            let j = t.inverse().apply(i);
            let (x, y) = (tree::X[j] as usize, tree::Y[j] as usize);

            if !board.is_valid(color, x, y) {
                policy[j] = ::std::f32::NEG_INFINITY;
            } else {
                policy[j] = original_policy[i];
            }
        }

        // get ride of symmetric moves, this is mostly useful for the opening.
        // Once we are past the first ~7 moves the board is usually sufficiently
        // asymmetric for this to turn into a no-op.
        //
        // we skip the first symmetry because it is the identity symmetry, which
        // is always a symmetry for any board.
        for &t in &SYMM[1..8] {
            if !symmetry::is_symmetric(board, t) {
                continue;
            }

            // figure out which are the useful vertices by eliminating the
            // symmetries from the board.
            let mut visited = [false; 368];

            for i in 0..361 {
                let j = t.apply(i);

                if i != j && !visited[i] {
                    visited[i] = true;
                    visited[j] = true;

                    let src = ::std::cmp::max(i, j);
                    let dst = ::std::cmp::min(i, j);

                    if policy[src].is_finite() {
                        assert!(policy[dst].is_finite());

                        policy[dst] += policy[src];
                        policy[src] = ::std::f32::NEG_INFINITY;
                    }
                }
            }
        }

        // renormalize the policy so that it sums to one after all the pruning that
        // we have performed.
        let policy_sum: f32 = policy.iter().filter(|p| p.is_finite()).sum();

        if policy_sum > 1e-6 {  // do not divide by zero
            let policy_recip = policy_sum.recip();

            for i in 0..362 {
                policy[i] *= policy_recip;
            }
        }

        Some((0.5 * value + 0.5, policy.into_boxed_slice()))
    })
}

/// The shared variables between the master and each worker thread in the `predict` function.
#[derive(Clone)]
struct ThreadContext<E: tree::Value + Clone + Send, T: TimeStrategy + Clone + Send> {
    /// The root of the monte carlo tree.
    root: Arc<UnsafeCell<tree::Node<E>>>,

    /// The initial board position at the root the tree.
    starting_point: Board,

    /// Time control element
    time_strategy: T,

    /// The number of probes that still needs to be done into the tree.
    remaining: Arc<AtomicIsize>,
}

unsafe impl<E: tree::Value + Clone + Send, T: TimeStrategy + Clone + Send> Send for ThreadContext<E, T> { }


/// Worker that probes into the given monte carlo search tree until the context
/// is exhausted.
/// 
/// # Arguments
/// 
/// * `context` - 
/// * `server` - 
/// 
fn predict_worker<E, T>(context: ThreadContext<E, T>, server: PredictGuard)
    where E: tree::Value + Clone + Send + 'static,
          T: TimeStrategy + Clone + Send + 'static
{
    let root = unsafe { &mut *context.root.get() };

    while !time_control::is_done(root, &context.time_strategy) {
        loop {
            let mut board = context.starting_point.clone();
            let trace = unsafe { tree::probe::<E>(root, &mut board) };

            if let Some(trace) = trace {
                let &(_, color, _) = trace.last().unwrap();
                let next_color = color.opposite();
                let result = forward(&server, &board, next_color);

                if let Some((value, policy)) = result {
                    unsafe {
                        tree::insert::<E>(&trace, next_color, value, policy);
                        break
                    }
                } else {
                    return  // unrecognized error
                }
            } else {
                server.send(PredictRequest::Wait);
            }
        }
    }
}

/// Predicts the _best_ next move according to the given neural network when applied
/// to a monte carlo tree search.
/// 
/// # Arguments
/// 
/// * `server` - the server to use during evaluation
/// * `num_workers` - 
/// * `starting_tree` - 
/// * `starting_point` - 
/// * `starting_color` - 
/// 
fn predict_aux<E, T>(
    server: &PredictGuard,
    num_workers: usize,
    time_strategy: T,
    starting_tree: Option<tree::Node<E>>,
    starting_point: &Board,
    starting_color: Color
) -> (f32, usize, tree::Node<E>)
    where E: tree::Value + Clone + Send + 'static,
          T: TimeStrategy + Clone + Send + 'static
{
    // if we have a starting tree given, then re-use that tree (after some sanity
    // checks), otherwise we need to query the neural network about what the
    // prior value should be at the root node.
    let mut starting_tree = if let Some(mut starting_tree) = starting_tree {
        assert_eq!(starting_tree.color, starting_color);

        if starting_tree.prior.iter().sum::<f32>() < 1e-4 {
            // we are missing the prior distribution, this can happend if we
            // fast-forwarded a passing move, but the pass move had not been
            // expanded (since we still need to create the node to record
            // that it was a pass so that we do not lose count of the number
            // of consecutive passes).
            let server = server.clone();
            let (_, policy) = forward(&server, starting_point, starting_color)
                .unwrap_or_else(|| {
                    let mut policy = vec! [0.0; 362];
                    policy[361] = 1.0;

                    (0.5, policy.into_boxed_slice())
                });

            for i in 0..362 {
                starting_tree.prior[i] = policy[i];
            }
        }

        starting_tree
    } else {
        let server = server.clone();
        let (value, mut policy) = forward(&server, starting_point, starting_color)
            .unwrap_or_else(|| {
                let mut policy = vec! [0.0; 362];
                policy[361] = 1.0;

                (0.5, policy.into_boxed_slice())
            });

        tree::Node::new(starting_color, value, policy)
    };

    // add some dirichlet noise to the root node of the search tree in order to increase
    // the entropy of the search and avoid overfitting to the prior value
    dirichlet::add(&mut starting_tree.prior, 0.03);

    // start-up all of the worker threads, and then start listening for requests on the
    // channel we gave each thread.
    let remaining = if *config::NUM_ROLLOUT > starting_tree.size() {
        (*config::NUM_ROLLOUT - starting_tree.size()) as isize
    } else {
        0
    };
    let context: ThreadContext<E, T> = ThreadContext {
        root: Arc::new(UnsafeCell::new(starting_tree)),
        starting_point: starting_point.clone(),

        time_strategy: time_strategy.clone(),
        remaining: Arc::new(AtomicIsize::new(remaining)),
    };

    let handles = (0..num_workers).map(|_| {
        let context = context.clone();
        let server = server.clone_static();

        thread::spawn(move || predict_worker::<E, T>(context, server))
    }).collect::<Vec<JoinHandle<()>>>();

    // wait for all threads to terminate to avoid any zombie processes
    for handle in handles.into_iter() { handle.join().unwrap(); }

    assert_eq!(Arc::strong_count(&context.root), 1);

    // choose the best move according to the search tree
    let root = UnsafeCell::into_inner(Arc::try_unwrap(context.root).ok().expect(""));
    let (value, index) = root.best(if starting_point.count() < 8 {
        *config::TEMPERATURE
    } else {
        0.0
    });

    #[cfg(feature = "trace-mcts")]
    eprintln!("{}", tree::to_sgf::<CGoban, E>(&root, starting_point, true));

    (value, index, root)
}

/// Predicts the _best_ next move according to the given neural network when applied
/// to a monte carlo tree search.
/// 
/// # Arguments
/// 
/// * `server` - the server to use during evaluation
/// * `num_workers` - 
/// * `starting_tree` -
/// * `starting_point` -
/// * `starting_color` -
/// 
pub fn predict<E, T>(
    server: &PredictGuard,
    num_workers: Option<usize>,
    time_control: T,
    starting_tree: Option<tree::Node<E>>,
    starting_point: &Board,
    starting_color: Color
) -> (f32, usize, tree::Node<E>)
    where E: tree::Value + Clone + Send + 'static,
          T: TimeStrategy + Clone + Send + 'static
{
    let num_workers = num_workers.unwrap_or(*config::NUM_THREADS);

    predict_aux::<E, T>(server, num_workers, time_control, starting_tree, starting_point, starting_color)
}

/// Play a game against the engine and return the result of the game.
/// 
/// # Arguments
/// 
/// * `server` - the server to use during evaluation
/// * `num_parallel` - the number of games that are being played in parallel
/// 
fn self_play_one(server: &PredictGuard, num_parallel: &Arc<AtomicUsize>) -> GameResult
{
    let mut board = Board::new();
    let mut sgf = String::new();
    let mut current = Color::Black;
    let mut pass_count = 0;
    let mut count = 0;

    // limit the maximum number of moves to `2 * 19 * 19` to avoid the
    // engine playing pointless capture sequences at the end of the game
    // that does not change the final result.
    let allow_resign = thread_rng().next_f32() < 0.95;
    let mut root = None;

    while count < 722 {
        let num_workers = *config::NUM_THREADS / num_parallel.load(Ordering::Acquire);
        let (value, index, tree) = predict_aux::<tree::DefaultValue, _>(
            &server,
            num_workers,
            RolloutLimit::new(*config::NUM_ROLLOUT),
            root,
            &board,
            current
        );

        debug_assert!(0.0 <= value && value <= 1.0);
        debug_assert!(index < 362);

        let policy = tree.softmax();
        let (_, prior_index) = tree.prior();
        let value_sgf = if current == Color::Black { 2.0 * value - 1.0 } else { -2.0 * value + 1.0 };

        if allow_resign && value < 0.05 {  // resign the game if the evaluation looks bad
            return GameResult::Resign(sgf, board, current.opposite(), -value);
        } else if index == 361 {  // passing move
            sgf += &format!(";{}[]P[{}]V[{}]", current, b85::encode(&policy), value_sgf);
            pass_count += 1;

            if pass_count >= 2 {
                return GameResult::Ended(sgf, board)
            }

            root = tree::Node::forward(tree, 361);
        } else {
            let (x, y) = (tree::X[index] as usize, tree::Y[index] as usize);

            sgf += &format!(";{}[{}]P[{}]V[{}]",
                current,
                CGoban::to_sgf(x, y),
                b85::encode(&policy),
                value_sgf
            );
            if prior_index != 361 {
                sgf += &format!("TR[{}]",
                    CGoban::to_sgf(
                        tree::X[prior_index] as usize,
                        tree::Y[prior_index] as usize
                    )
                );
            };

            pass_count = 0;
            board.place(current, x, y);
            root = tree::Node::forward(tree, index);
        }

        current = current.opposite();
        count += 1;
    }

    GameResult::Ended(sgf, board)
}

/// Play games against the engine and return the result of the games
/// over the channel.
/// 
/// # Arguments
/// 
/// * `network` - the neural network to use during evaluation
/// * `num_games` - the number of games to generate
/// 
pub fn self_play(network: Network, num_games: usize) -> (Receiver<GameResult>, PredictService) {
    let server = predict::service(network);
    let (sender, receiver) = channel();

    // spawn the worker threads that generate the self-play games
    let num_parallel = ::std::cmp::min(num_games, *config::NUM_GAMES);
    let num_workers = Arc::new(AtomicUsize::new(num_parallel));
    let processed = Arc::new(AtomicUsize::new(0));

    for _ in 0..num_parallel {
        let num_workers = num_workers.clone();
        let processed = processed.clone();
        let sender = sender.clone();
        let server = server.lock().clone_static();

        thread::spawn(move || {
            while processed.fetch_add(1, Ordering::SeqCst) < num_games {
                let result = self_play_one(&server, &num_workers);

                if sender.send(result).is_err() {
                    break
                }
            }

            num_workers.fetch_sub(1, Ordering::Release);
        });
    }

    (receiver, server)
}

/// Play a game against the engine and return the result of the game.
/// This is different from `self_play` because this method does not
/// perform any search and only plays stochastically according
/// to the policy network.
/// 
/// # Arguments
/// 
/// * `server` - the server to use during evaluation
/// 
fn policy_play_one(server: &PredictGuard) -> GameResult {
    let mut temperature = (*config::TEMPERATURE + 1e-3).recip();
    let mut board = Board::new();
    let mut sgf = String::new();
    let mut current = Color::Black;
    let mut pass_count = 0;
    let mut count = 0;

    while pass_count < 2 && count < 722 && !board.is_scoreable() {
        let result = forward(&server, &board, current);
        if result.is_none() {
            break
        }

        let (_, policy) = result.unwrap();

        // pick a move stochastically according to its prior value with the
        // specified temperature (to priority strongly suggested moves, and
        // avoid picking _noise_ moves).
        let index = {
            let policy_sum = policy.iter()
                .filter(|p| p.is_finite())
                .map(|p| p.powf(temperature))
                .sum::<f32>();
            let threshold = policy_sum * thread_rng().next_f32();
            let mut so_far = 0.0f32;
            let mut best = None;

            for i in 0..362 {
                if policy[i].is_finite() {
                    so_far += policy[i].powf(temperature);

                    if so_far >= threshold {
                        best = Some(i);
                        break
                    }
                }
            }

            best  // if nothing, then pass
        };

        if let Some(index) = index {
            if index == 361 {  // pass
                sgf += &format!(";{}[]", current);
                pass_count += 1;
            } else {  // normal move
                let (x, y) = (tree::X[index] as usize, tree::Y[index] as usize);

                sgf += &format!(";{}[{}]", current, CGoban::to_sgf(x, y));
                pass_count = 0;
                board.place(current, x, y);
            }
        } else {  // no valid moves remaining
            sgf += &format!(";{}[]", current);
            pass_count += 1;
        }

        // continue with the next turn
        temperature = min(5.0, 1.03 * temperature);
        current = current.opposite();
        count += 1;
    }

    // if the receiver has terminated then quit
    GameResult::Ended(sgf, board)
}

/// Play games against the engine and return the results of the game over
/// the returned channel. This is different from `self_play` because this
/// method does not perform any search and only plays stochastically according
/// to the policy network.
/// 
/// # Arguments
/// 
/// * `network` - the neural network to use during evaluation
/// * `num_games` - 
/// 
pub fn policy_play(network: Network, num_games: usize) -> (Receiver<GameResult>, PredictService) {
    let server = predict::service(network);
    let (sender, receiver) = channel();

    // spawn the worker threads that generate the self-play games
    let num_workers = ::std::cmp::min(*config::NUM_GAMES, num_games);
    let remaining = Arc::new(AtomicUsize::new(num_games));

    for _ in 0..num_workers {
        let remaining = remaining.clone();
        let sender = sender.clone();
        let server = server.lock().clone_static();

        thread::spawn(move || {
            while remaining.load(Ordering::Acquire) > 0 {
                remaining.fetch_sub(1, Ordering::AcqRel);

                let result = policy_play_one(&server);

                if sender.send(result).is_err() {
                    break
                }
            }
        });
    }

    (receiver, server)
}

/// Play the given board until the end using the policy of the neural network
/// in a greedy manner (ignoring the pass move every time) until it is scoreable
/// according to the TT-rules.
/// 
/// # Arguments
/// 
/// * `server` - the server to use during evaluation
/// * `board` - the board to score
/// * `next_color` - the color of the player whose turn it is to play
/// 
pub fn greedy_score(server: &PredictGuard, board: &Board, next_color: Color) -> (Board, String) {
    let mut board = board.clone();
    let mut sgf = String::new();
    let mut current = next_color;
    let mut pass_count = 0;
    let mut count = 0;

    while count < 722 && pass_count < 2 && !board.is_scoreable() {
        let result = forward(&server, &board, current);
        if result.is_none() {
            break
        }

        let (_, policy) = result.unwrap();

        // pick a move stochastically according to its prior value with the
        // specified temperature (to priority strongly suggested moves, and
        // avoid picking _noise_ moves).
        let index = (0..361)
            .filter(|&i| policy[i].is_finite())
            .max_by_key(|&i| OrderedFloat(policy[i]));

        if let Some(index) = index {
            let (x, y) = (tree::X[index] as usize, tree::Y[index] as usize);

            sgf += &format!(";{}[{}]", current, Sabaki::to_sgf(x, y));
            pass_count = 0;
            board.place(current, x, y);
        } else {  // no valid moves remaining
            sgf += &format!(";{}[]", current);
            pass_count += 1;
        }

        // continue with the next turn
        current = current.opposite();
        count += 1;
    }

    (board, sgf)
}

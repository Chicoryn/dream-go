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

use dg_go::utils::benson::BensonImpl;
use dg_go::{Board, Color, Point, IsPartOf};

pub trait PolicyChecker {
    /// Returns true if the given move should be considered during search.
    ///
    /// # Arguments
    ///
    /// * `board` -
    /// * `point` -
    ///
    fn is_policy_candidate(&self, board: &Board, point: Point) -> bool;
}

pub trait SearchOptions {
    /// Returns the policy checker to use for the given `board`.
    ///
    /// # Arguments
    ///
    /// * `board` -
    /// * `to_move` -
    ///
    fn policy_checker(&self, board: &Board, to_move: Color) -> Box<dyn PolicyChecker>;

    /// Returns true if the search should be deterministic.
    fn deterministic(&self) -> bool;
}

pub struct StandardPolicyChecker {
    to_move: Color
}

impl StandardPolicyChecker {
    fn new(to_move: Color) -> Self {
        Self { to_move }
    }
}

impl PolicyChecker for StandardPolicyChecker {
    fn is_policy_candidate(&self, board: &Board, point: Point) -> bool {
        point == Point::default() || board.is_valid(self.to_move, point)
    }
}

#[derive(Clone)]
pub struct StandardSearch;

impl StandardSearch {
    pub fn new() -> Self {
        Self { }
    }
}

impl Default for StandardSearch {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchOptions for StandardSearch {
    fn policy_checker(&self, _board: &Board, to_move: Color) -> Box<dyn PolicyChecker> {
        Box::new(StandardPolicyChecker::new(to_move))
    }

    fn deterministic(&self) -> bool {
        false
    }
}

#[derive(Clone)]
pub struct StandardDeterministicSearch;

impl StandardDeterministicSearch {
    pub fn new() -> Self {
        Self { }
    }
}

impl Default for StandardDeterministicSearch {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchOptions for StandardDeterministicSearch {
    fn policy_checker(&self, _board: &Board, to_move: Color) -> Box<dyn PolicyChecker> {
        Box::new(StandardPolicyChecker::new(to_move))
    }

    fn deterministic(&self) -> bool {
        true
    }
}

pub struct ScoringPolicyChecker {
    is_valid: [bool; Point::MAX],
    to_move: Color
}

impl ScoringPolicyChecker {
    fn new(board: &Board, to_move: Color) -> ScoringPolicyChecker {
        let benson_black = BensonImpl::new(board, Color::Black);
        let benson_white = BensonImpl::new(board, Color::White);
        let mut out = Self {
            is_valid: [false; Point::MAX],
            to_move: to_move
        };

        for point in Point::all() {
            out.is_valid[point] = !benson_black.is_eye(point) && !benson_white.is_eye(point);
        }

        out
    }
}

impl PolicyChecker for ScoringPolicyChecker {
    fn is_policy_candidate(&self, board: &Board, point: Point) -> bool {
        point != Point::default() &&
            self.is_valid[point] &&
            board.is_valid(self.to_move, point) &&
            !is_eye(&board, self.to_move, point)
    }
}

#[derive(Clone)]
pub struct ScoringSearch;

impl ScoringSearch {
    pub fn new() -> Self {
        Self { }
    }
}

impl Default for ScoringSearch {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchOptions for ScoringSearch {
    fn policy_checker(&self, board: &Board, to_move: Color) -> Box<dyn PolicyChecker> {
        Box::new(ScoringPolicyChecker::new(board, to_move))
    }

    fn deterministic(&self) -> bool {
        true
    }
}

/// Returns true if the given vertex is is occupied by a stone of the same color.
///
/// # Arguments
///
/// * `board` -
/// * `color` -
/// * `point` -
/// * `dx` -
/// * `dy` -
///
fn is_vertex_filled(board: &Board, color: Color, point: Point, dx: i8, dy: i8) -> bool {
    let other = point.offset(dx as isize, dy as isize);

    board.is_part_of(other) && board.at(other) == Some(color)
}

/// Returns true if the given move would fill ones own eye. An eye in this case
/// is recognized as an empty spot that is surrounded by at least 7 stones of
/// the same color. This will miss some _complicated_ eyes, but this is good
/// enough for the heuristic.
///
/// # Arguments
///
/// * `board` -
/// * `color` -
/// * `point` -
///
fn is_eye(board: &Board, color: Color, point: Point) -> bool {
    const CROSS: [(i8, i8); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];
    const DIAGONAL: [(i8, i8); 4] = [(1, 1), (1, -1), (-1, 1), (-1, -1)];

    let num_cross = CROSS.iter()
        .filter(|(dx, dy)| is_vertex_filled(board, color, point, *dx, *dy))
        .count();
    let num_diagonal = DIAGONAL.iter()
        .filter(|(dx, dy)| is_vertex_filled(board, color, point, *dx, *dy))
        .count();

    // distinguish between the three different cases, (i) an eye in the middle,
    // (ii) an eye in along the edge, and (iii) an eye in the corner.
    let (x, y) = (point.x(), point.y());

    if (x == 0 || x == 18) && (y == 0 || y == 18) {
        num_cross >= 2 && num_diagonal >= 1  // corner move
    } else if x == 0 || x == 18 || y == 0 || y == 18 {
        num_cross >= 3 && num_diagonal >= 2  // edge
    } else {
        num_cross >= 4 && num_diagonal >= 3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corner() {
        let mut board = Board::new(0.5);
        board.place(Color::Black, Point::new(1, 0));
        board.place(Color::Black, Point::new(0, 1));
        board.place(Color::Black, Point::new(1, 1));

        assert!(is_eye(&board, Color::Black, Point::new(0, 0)));
        assert!(!is_eye(&board, Color::White, Point::new(0, 0)));
    }

    #[test]
    fn side() {
        let mut board = Board::new(0.5);
        board.place(Color::Black, Point::new(0, 0));
        board.place(Color::Black, Point::new(0, 1));
        board.place(Color::Black, Point::new(1, 1));
        board.place(Color::Black, Point::new(2, 1));
        board.place(Color::Black, Point::new(2, 0));

        assert!(is_eye(&board, Color::Black, Point::new(1, 0)));
        assert!(!is_eye(&board, Color::White, Point::new(1, 0)));
    }

    #[test]
    fn middle() {
        let mut board = Board::new(0.5);
        board.place(Color::Black, Point::new(0, 1));
        board.place(Color::Black, Point::new(0, 2));
        board.place(Color::Black, Point::new(1, 0));
        board.place(Color::Black, Point::new(2, 0));
        board.place(Color::Black, Point::new(2, 2));
        board.place(Color::Black, Point::new(2, 1));
        board.place(Color::Black, Point::new(1, 2));

        assert!(is_eye(&board, Color::Black, Point::new(1, 1)), "{}", board);
        assert!(!is_eye(&board, Color::White, Point::new(1, 1)), "{}", board);

        board.place(Color::Black, Point::new(0, 0));

        assert!(is_eye(&board, Color::Black, Point::new(1, 1)), "{}", board);
        assert!(!is_eye(&board, Color::White, Point::new(1, 1)), "{}", board);
    }
}
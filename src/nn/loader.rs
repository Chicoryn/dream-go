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
use std::fs::File;
use std::intrinsics::unlikely;
use std::io::{BufReader, Read};
use std::path::Path;
use std::char;

use nn::tensor::Tensor;
use util::b85;

/// Step the iterator forward until the character given `stop` character is
/// encountered. The character `stop` is also skipped.
/// 
/// # Arguments
/// 
/// * `iter` - the iterator to step forward
/// * `stop` - the character to step until
/// 
fn skip_until<I>(iter: &mut I, stop: char) -> String
    where I: Iterator<Item=u8>
{
    let mut out: String = String::new();

    loop {
        if let Some(ch) = iter.next() {
            let ch = char::from_u32(ch as u32).unwrap();

            if unsafe { unlikely(ch == stop) } {
                break
            }

            out.push(ch);
        } else {
            break
        }
    }

    out
}

/// An iterator that parse entries with the following format:
/// 
/// `"name": { "s": "...", v: "..." }`
/// 
struct JsonEntryIter<I: Iterator<Item=u8>> {
    iter: I
}

impl<I: Iterator<Item=u8>> Iterator for JsonEntryIter<I> {
    type Item = (String, Tensor);

    fn next(&mut self) -> Option<Self::Item> {
        // skip until the quote before the name
        skip_until(&mut self.iter, '"');

        let name = skip_until(&mut self.iter, '"');
        if name.is_empty() {
            return None;
        }

        // skip until the next `{` and then parse the interior of the
        // object by iterating over the properties
        skip_until(&mut self.iter, '{');

        let mut tensor = Tensor::default();

        loop {
            skip_until(&mut self.iter, '"');
            let key = skip_until(&mut self.iter, '"');

            skip_until(&mut self.iter, '"');
            let value = skip_until(&mut self.iter, '"');

            if key == "s" {
                let array = b85::decode::<f32, _>(&value).unwrap();

                tensor.scale = array[0];
            } else if key == "v" {
                tensor.set_host(b85::decode::<u8, _>(&value).unwrap());
            } else {
                break
            }

            // check if the object terminated
            let more = skip_until(&mut self.iter, ',');
            if more.contains('}') {
                break
            }
        };

        Some((name, tensor))
    }
}

/// Load all tensors in the given file and returns a map from
/// their name to description. If we failed to load any tensors
/// from the given file then `None` is returned.
/// 
/// # Arguments
/// 
/// * `path` -
/// 
pub fn load(path: &Path) -> Option<HashMap<String, Tensor>> {
    if let Ok(file) = File::open(path) {
        let mut out: HashMap<String, Tensor> = HashMap::new();
        let mut iter = JsonEntryIter {
            iter: BufReader::new(file).bytes().map(|ch| ch.unwrap())
        };

        for (name, t) in iter {
            debug_assert!(t.scale > 0.0, "scale is non-positive for layer {} -- {}", name, t.scale);

            out.insert(name, t);
        }

        Some(out)
    } else {
        None
    }
}

/*
#[cfg(test)]
mod tests {
    use std::path::Path;
    use test::{Bencher};

    use nn::loader::load;

    #[bench]
    fn load_json(b: &mut Bencher) {
        b.iter(|| {
            let out = load(Path::new("dream_go.json"));

            assert!(out.is_some());
            out
        });
    }
}
*/

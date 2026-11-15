//! Pipes mode: a few pipes grow at once through a 3D grid. The CPU walks the grid; every step
//! becomes one GPU instance (a straight tube, a quarter-torus elbow or a ball) that the vertex
//! shader `vs_pipe` grows over time. When the space is full, the scene fades out and restarts.

pub const N: i32 = 12; // grid cells per side
pub const STEP: f32 = 0.22; // seconds per cell, must match PSTEP in the shader
const PIPES: usize = 4;
const FULL: f32 = 0.4; // fraction of cells filled before restarting
const MAX_CYCLE: f32 = 60.0;
const FADE: f32 = 2.0;
/// Upper bound on pieces per cycle: one per cell plus start/end pieces and balls.
pub const CAPACITY: usize = (N * N * N) as usize * 2;

const DIRS: [[i32; 3]; 6] = [[1, 0, 0], [-1, 0, 0], [0, 1, 0], [0, -1, 0], [0, 0, 1], [0, 0, -1]];
pub const KIND_CELL: u32 = 0; // from the entry midpoint through the cell: straight or elbow
pub const KIND_START: u32 = 1; // half tube from the cell centre out along d_out
pub const KIND_END: u32 = 2; // half tube from the entry midpoint to the centre
pub const KIND_BALL: u32 = 3;

/// One GPU instance (32 bytes): world position + birth time, then d_in, d_out, kind, colour.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Piece {
    pub pos: [f32; 3],
    pub born: f32,
    pub d_in: u32,
    pub d_out: u32,
    pub kind: u32,
    pub color: u32,
}

impl Piece {
    pub fn bytes(pieces: &[Piece]) -> Vec<u8> {
        let mut b = Vec::with_capacity(pieces.len() * 32);
        for p in pieces {
            for f in [p.pos[0], p.pos[1], p.pos[2], p.born] {
                b.extend(f.to_ne_bytes());
            }
            for u in [p.d_in, p.d_out, p.kind, p.color] {
                b.extend(u.to_ne_bytes());
            }
        }
        b
    }
}

struct Pipe {
    head: [i32; 3],
    dir: usize,
    color: u32,
    next: f32, // time of the next step
}

pub struct Pipes {
    rng: u64,
    occupied: Vec<bool>,
    filled: usize,
    pipes: Vec<Pipe>,
    pub pieces: Vec<Piece>,
    cycle_start: f32,
    ending: Option<f32>,
}

fn world(c: [i32; 3]) -> [f32; 3] {
    let h = (N - 1) as f32 / 2.0;
    [c[0] as f32 - h, c[1] as f32 - h, c[2] as f32 - h]
}

fn add(a: [i32; 3], d: usize) -> [i32; 3] {
    [a[0] + DIRS[d][0], a[1] + DIRS[d][1], a[2] + DIRS[d][2]]
}

fn half_back(c: [i32; 3], d: usize) -> [f32; 3] {
    let w = world(c);
    [w[0] - DIRS[d][0] as f32 * 0.5, w[1] - DIRS[d][1] as f32 * 0.5, w[2] - DIRS[d][2] as f32 * 0.5]
}

impl Pipes {
    pub fn new(seed: u64) -> Self {
        let mut p = Self { rng: seed | 1, occupied: Vec::new(), filled: 0, pipes: Vec::new(), pieces: Vec::new(), cycle_start: 0.0, ending: None };
        p.reset(0.0);
        p
    }

    fn reset(&mut self, t: f32) {
        self.occupied = vec![false; (N * N * N) as usize];
        self.filled = 0;
        self.pipes.clear();
        self.pieces.clear();
        self.cycle_start = t;
        self.ending = None;
    }

    fn rand(&mut self, n: usize) -> usize {
        crate::render::xorshift(&mut self.rng) as usize % n
    }

    fn free(&self, c: [i32; 3]) -> bool {
        c.iter().all(|&v| (0..N).contains(&v)) && !self.occupied[((c[0] * N + c[1]) * N + c[2]) as usize]
    }

    fn take(&mut self, c: [i32; 3]) {
        self.occupied[((c[0] * N + c[1]) * N + c[2]) as usize] = true;
        self.filled += 1;
    }

    fn free_dirs(&self, c: [i32; 3]) -> Vec<usize> {
        (0..6).filter(|&d| self.free(add(c, d))).collect()
    }

    /// Starts a pipe at a random free cell with room to grow. False if none was found.
    fn spawn(&mut self, t: f32) -> bool {
        for _ in 0..40 {
            let c = [self.rand(N as usize) as i32, self.rand(N as usize) as i32, self.rand(N as usize) as i32];
            if !self.free(c) {
                continue;
            }
            let dirs = self.free_dirs(c);
            if dirs.is_empty() {
                continue;
            }
            let d = dirs[self.rand(dirs.len())];
            let color = self.rand(8) as u32;
            self.take(c);
            let next = add(c, d);
            self.take(next); // reserved now so no other pipe grows into it
            let w = world(c);
            self.pieces.push(Piece { pos: w, born: t, d_in: d as u32, d_out: d as u32, kind: KIND_BALL, color });
            self.pieces.push(Piece { pos: w, born: t, d_in: d as u32, d_out: d as u32, kind: KIND_START, color });
            self.pipes.push(Pipe { head: next, dir: d, color, next: t + STEP * 0.5 });
            return true;
        }
        false
    }

    /// Grows pipe `i` by one cell. False when it got stuck and ended.
    fn step(&mut self, i: usize) -> bool {
        let Pipe { head, dir, color, next } = self.pipes[i];
        let dirs = self.free_dirs(head);
        let pos = half_back(head, dir);
        if dirs.is_empty() {
            self.pieces.push(Piece { pos, born: next, d_in: dir as u32, d_out: dir as u32, kind: KIND_END, color });
            self.pieces.push(Piece { pos: world(head), born: next + STEP * 0.5, d_in: dir as u32, d_out: dir as u32, kind: KIND_BALL, color });
            return false;
        }
        let out = if dirs.contains(&dir) && self.rand(4) != 0 { dir } else { dirs[self.rand(dirs.len())] };
        self.pieces.push(Piece { pos, born: next, d_in: dir as u32, d_out: out as u32, kind: KIND_CELL, color });
        let h = add(head, out);
        self.take(h);
        self.pipes[i] = Pipe { head: h, dir: out, color, next: next + STEP };
        true
    }

    /// Advances the simulation to time `t` (seconds since the mode started).
    pub fn update(&mut self, t: f32) {
        if let Some(e) = self.ending {
            if t >= e + FADE {
                self.reset(t);
            }
            return;
        }
        while self.pipes.len() < PIPES && self.spawn(t) {}
        let mut i = 0;
        while i < self.pipes.len() {
            if self.pipes[i].next > t {
                i += 1;
            } else if !self.step(i) {
                self.pipes.swap_remove(i);
            }
        }
        let full = self.filled as f32 >= FULL * (N * N * N) as f32;
        if full || self.pipes.is_empty() || t - self.cycle_start > MAX_CYCLE {
            self.ending = Some(t + STEP); // let the last pieces finish growing
        }
    }

    /// Scene opacity: 1 while growing, easing to 0 before a restart.
    pub fn fade(&self, t: f32) -> f32 {
        self.ending.map_or(1.0, |e| {
            let x = ((t - e) / FADE).clamp(0.0, 1.0);
            0.5 + 0.5 * (std::f32::consts::PI * x).cos()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn key(p: [f32; 3]) -> [i32; 3] {
        p.map(|v| (v * 2.0).round() as i32) // half-cell resolution
    }

    fn end_of(p: &Piece) -> Option<[f32; 3]> {
        let d = DIRS[p.d_out as usize].map(|v| v as f32 * 0.5);
        let centre = |p: &Piece| {
            let d = DIRS[p.d_in as usize].map(|v| v as f32 * 0.5);
            [p.pos[0] + d[0], p.pos[1] + d[1], p.pos[2] + d[2]]
        };
        match p.kind {
            KIND_START => Some([p.pos[0] + d[0], p.pos[1] + d[1], p.pos[2] + d[2]]),
            KIND_CELL => {
                let c = centre(p);
                Some([c[0] + d[0], c[1] + d[1], c[2] + d[2]])
            }
            _ => None,
        }
    }

    /// Checks one finished cycle: pipes connect, never share a cell, stay inside the grid.
    fn check_cycle(pieces: &[Piece]) {
        let ends: HashSet<[i32; 3]> = pieces.iter().filter_map(end_of).map(key).collect();
        let mut cells = HashSet::new();
        let lim = (N - 1) as f32 / 2.0;
        for p in pieces {
            let centre = match p.kind {
                KIND_START | KIND_BALL => p.pos,
                _ => {
                    assert!(ends.contains(&key(p.pos)), "piece starts where no pipe ended: {p:?}");
                    let d = DIRS[p.d_in as usize].map(|v| v as f32 * 0.5);
                    [p.pos[0] + d[0], p.pos[1] + d[1], p.pos[2] + d[2]]
                }
            };
            assert!(centre.iter().all(|v| v.abs() <= lim + 1e-4), "outside the grid: {p:?}");
            if p.kind != KIND_BALL {
                assert!(cells.insert(key(centre)), "two pipes in one cell: {p:?}");
            }
            if p.kind == KIND_CELL && p.d_in != p.d_out {
                assert_ne!(p.d_in ^ 1, p.d_out, "a pipe never turns back on itself");
            }
        }
    }

    #[test]
    fn pipes_connect_and_never_overlap_then_restart() {
        for seed in [1u64, 7, 42, 999] {
            let mut p = Pipes::new(seed);
            let (mut restarts, mut last) = (0, Vec::new());
            for f in 0..60 * 200 {
                p.update(f as f32 / 60.0);
                assert!(p.pieces.len() <= CAPACITY);
                if p.pieces.len() < last.len() {
                    check_cycle(&last);
                    restarts += 1;
                }
                last.clone_from(&p.pieces);
            }
            assert!(restarts >= 2, "seed {seed}: only {restarts} restarts in 200 s");
        }
    }

    #[test]
    fn grows_several_pipes_at_once() {
        let mut p = Pipes::new(3);
        p.update(0.0);
        assert_eq!(p.pipes.len(), PIPES);
        assert_eq!(p.fade(0.0), 1.0);
    }
}

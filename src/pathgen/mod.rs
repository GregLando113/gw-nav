//! Pathing generation: a port of the client's path bloat
//! (`Engine\Map\Path\*.cpp`), which builds the navmesh from the terrain,
//! props and collision data of a stage-1 map file.
//!
//! The goal is output identical to the client's, so float operations follow
//! the client's instruction order and precision. See the "How the client
//! generates the path planes" section of `docs/mapblob-format.md`.

pub mod assemble;
pub mod build;
pub mod chunk;
pub mod clip;
pub mod fastmath;
pub mod obstacles;
pub mod props;
pub mod random;
#[cfg(test)]
pub(crate) mod testing;
pub mod tracer;

/// Precision of the client's x87 arithmetic. Intermediate results of x87
/// instructions are rounded to this precision; stores to f32 variables
/// always round to f32.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X87 {
    /// Precision control set to 24 bits (Direct3D's default).
    Single,
    /// Precision control set to 53 bits (the MSVC runtime default). This
    /// is what the client's bloat uses: prop collision z values only match
    /// the client's in this mode.
    Double,
}

impl X87 {
    /// Round the result of one x87 operation.
    #[inline]
    pub fn r(self, v: f64) -> f64 {
        match self {
            Self::Single => v as f32 as f64,
            Self::Double => v,
        }
    }

    /// Store to an f32 variable.
    #[inline]
    pub fn f32(self, v: f64) -> f32 {
        self.r(v) as f32
    }
}

/// The client's `Math_Sin` (misnamed): round half away from zero via
/// `trunc(f32(x ± 0.5))`.
pub fn round(x: f32) -> i32 {
    let half = if x.is_sign_negative() { -0.5 } else { 0.5 };
    (x as f64 + half) as f32 as i32
}

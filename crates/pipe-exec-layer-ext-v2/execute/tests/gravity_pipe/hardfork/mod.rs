//! Scenarios, one module per hardfork. Each module decides what goes into the blocks
//! of its phases (before activation, the activation block, after activation) and what
//! to assert once they are committed.

mod alpha;
mod base;
mod beta;
mod gamma;
mod prague;

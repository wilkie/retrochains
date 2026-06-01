//! MSC code generation, split by concern. Type definitions
//! live in the crate root (`lib.rs`); these modules hold the
//! emission free functions.

mod assign;
mod calls;
mod cond;
mod constprop;
mod expr;
mod func;
mod statements;

pub(crate) use assign::*;
pub(crate) use calls::*;
pub(crate) use cond::*;
pub(crate) use constprop::*;
pub(crate) use expr::*;
pub(crate) use func::*;
pub(crate) use statements::*;

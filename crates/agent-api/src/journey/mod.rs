//! B8: industry journeys (themes, stages, activities), their machine rules (11§7) and the grid loader. T2a is pure:
//! [`model`] (wire shapes, open-set stage/theme slugs B8-10) and [`rules`]; T2b adds [`grid`] (the grid on the KH
//! fold: `load`, `load_at`, `industries`).
pub mod grid;
pub mod model;
pub mod refine;
pub mod rules;

pub mod deps;
pub mod designer;
pub mod job;

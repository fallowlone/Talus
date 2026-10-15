//! Printing of diagrams (the ER diagram and the data model): a device-independent [`Scene`] is paginated onto
//! sheets of paper ([`layout`]) and written as a PDF ([`pdf`]) whose text is vector outlines, so any
//! language the application's font covers prints crisply and no font has to be installed or embedded.
//! The same scene and layout drive the on-screen preview, so what is previewed is what is printed.

pub mod font;
pub mod layout;
pub mod pdf;
pub mod scene;

pub use font::{Fonts, GlyphOutline, PathOp};
pub use layout::{Fit, Layout, PageSetup, Paper, paginate};
pub use pdf::{Meta, render_pdf};
pub use scene::{Align, Card, CardRow, FontStyle, Link, Marker, Prim, Rgb, Scene, diagram_scene};

//! Spotitogether Drive: a 3D car game drawn in the terminal, on real
//! places.
//!
//! - [`picker`]: choosing the place on an ASCII-art world map ([`tiles`]
//!   for the elevation);
//! - [`osm`] and [`map`]: the place as data (relief, land cover, roads,
//!   buildings, trees), [`world`] and [`mesh`]: the same as triangles;
//! - [`raster`], [`lighting`], [`material`], [`present`]: the software
//!   renderer and the two terminal looks;
//! - [`physics`] and [`input`]: the car and its controls; [`demo`]: the
//!   game loop.

pub mod ascii_map;
pub mod demo;
pub mod input;
pub mod lighting;
pub mod map;
pub mod material;
pub mod mesh;
pub mod mvt;
pub mod noise;
pub mod osm;
pub mod physics;
pub mod present;
pub mod picker;
pub mod raster;
pub mod tiles;
pub mod world;

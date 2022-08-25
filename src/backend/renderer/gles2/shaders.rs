/*
 * OpenGL Shaders
 */
pub const VERTEX_SHADER: &str = include_str!("./shaders/textured/texture.vert");

pub const FRAGMENT_COUNT: usize = 3;

pub const FRAGMENT_SHADER_ABGR: &str = include_str!("./shaders/textured/abgr.frag");
pub const FRAGMENT_SHADER_XBGR: &str = include_str!("./shaders/textured/xbgr.frag");
pub const FRAGMENT_SHADER_EXTERNAL: &str = include_str!("./shaders/textured/external.frag");

pub const VERTEX_SHADER_SOLID: &str = include_str!("./shaders/solid/solid.vert");
pub const FRAGMENT_SHADER_SOLID: &str = include_str!("./shaders/solid/solid.vert");

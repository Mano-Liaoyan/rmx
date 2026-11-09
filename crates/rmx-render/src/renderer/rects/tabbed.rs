//! Rects of panes drawn in a [`Frame`](crate::renderer::Frame) rather than from the window's
//! bottom left corner.

use super::RectShaderProgram;
use crate::SizeInfo;
use crate::gl;

impl RectShaderProgram {
    /// Align line patterns with cells whose grid starts at `[x, y]`, the window pixel where
    /// `size_info` is drawn from its bottom left.
    pub(super) fn update_origin(&self, size_info: &SizeInfo, [x, y]: [f32; 2]) {
        let viewport_height = size_info.height() - size_info.padding_y();
        let padding_y = viewport_height
            - (viewport_height / size_info.cell_height()).floor() * size_info.cell_height();

        unsafe {
            if let Some(u_padding_y) = self.u_padding_y {
                gl::Uniform1f(u_padding_y, y + padding_y);
            }
            if let Some(u_padding_x) = self.u_padding_x {
                gl::Uniform1f(u_padding_x, x + size_info.padding_x());
            }
        }
    }
}

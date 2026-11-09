#if defined(GLES2_RENDERER)
attribute vec2 aPos;

varying vec2 uv;
#else
layout (location = 0) in vec2 aPos;

out vec2 uv;
#endif

// Sampled rectangle of the texture: left, top, right, bottom. Row zero of the texture is the top
// row of the remote desktop, so the top of the quad takes the smaller v.
uniform vec4 source;

void main() {
    vec2 corner = aPos * 0.5 + 0.5;
    uv = vec2(mix(source.x, source.z, corner.x), mix(source.w, source.y, corner.y));
    gl_Position = vec4(aPos, 0.0, 1.0);
}

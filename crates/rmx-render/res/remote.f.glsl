#if defined(GLES2_RENDERER)
// 16 bit floats cannot address the texels of a 4K texture.
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif

varying vec2 uv;

#define FRAG_COLOR gl_FragColor
#define TEXTURE texture2D
#else
in vec2 uv;

out vec4 fragColor;

#define FRAG_COLOR fragColor
#define TEXTURE texture
#endif

uniform sampler2D tex;

// Distance of the four samples from the pixel's centre, in texture coordinates. Zero unless the
// picture is drawn smaller than it is: bilinear filtering alone skips texels past 2:1.
uniform vec2 tap;

// 1 where BGRA pixels were uploaded as if they were RGBA, as GLES 2 has no BGRA upload.
uniform float swapRedBlue;

void main() {
    vec3 color = (TEXTURE(tex, uv + vec2(-tap.x, -tap.y)).rgb
            + TEXTURE(tex, uv + vec2(tap.x, -tap.y)).rgb
            + TEXTURE(tex, uv + vec2(-tap.x, tap.y)).rgb
            + TEXTURE(tex, uv + vec2(tap.x, tap.y)).rgb) * 0.25;

    // Opaque whatever the fourth byte is: remote servers leave it unset, often zero, and the
    // window may be translucent.
    FRAG_COLOR = vec4(mix(color, color.bgr, swapRedBlue), 1.0);
}

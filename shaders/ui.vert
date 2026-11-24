#version 450
// One instance = one quad: a rounded rectangle, a shadow, a glyph or an image.
layout(push_constant) uniform Push { vec2 screen; } push;

layout(location = 0) in vec4 rect;    // x, y, w, h in pixels
layout(location = 1) in vec4 uv;      // atlas u0, v0, u1, v1 (glyphs, images)
layout(location = 2) in vec4 color;   // RGBA8 unorm
layout(location = 3) in vec3 params;  // corner radius, border width or blur, mode (0 rect, 1 glyph, 2 image, 3 shadow)

layout(location = 0) out vec2 v_local;
layout(location = 1) out vec2 v_uv;
layout(location = 2) out vec4 v_color;
layout(location = 3) flat out vec3 v_params;
layout(location = 4) flat out vec2 v_half;

const vec2 corners[6] = vec2[](
    vec2(0, 0), vec2(1, 0), vec2(0, 1),
    vec2(0, 1), vec2(1, 0), vec2(1, 1));

void main() {
    vec2 c = corners[gl_VertexIndex];
    vec2 pos = rect.xy + c * rect.zw;
    gl_Position = vec4(pos / push.screen * 2.0 - 1.0, 0.0, 1.0);
    v_local = (c - 0.5) * rect.zw;
    v_half = rect.zw * 0.5;
    v_uv = mix(uv.xy, uv.zw, c);
    v_color = color;
    v_params = params;
}

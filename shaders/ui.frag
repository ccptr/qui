#version 450
layout(set = 0, binding = 0) uniform sampler2D atlas;

layout(location = 0) in vec2 v_local;
layout(location = 1) in vec2 v_uv;
layout(location = 2) in vec4 v_color;
layout(location = 3) flat in vec3 v_params;
layout(location = 4) flat in vec2 v_half;

layout(location = 0) out vec4 out_color;

void main() {
    if (v_params.z > 0.5) {
        float coverage = texture(atlas, v_uv).r;
        out_color = vec4(v_color.rgb, v_color.a * coverage);
        return;
    }
    // Signed distance to the rounded rectangle, anti-aliased over one pixel.
    float r = min(v_params.x, min(v_half.x, v_half.y));
    vec2 q = abs(v_local) - v_half + r;
    float d = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
    float alpha = clamp(0.5 - d, 0.0, 1.0);
    float border = v_params.y;
    if (border > 0.0) {
        alpha *= clamp(d + border + 0.5, 0.0, 1.0);
    }
    out_color = vec4(v_color.rgb, v_color.a * alpha);
}

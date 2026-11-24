#version 450
layout(set = 0, binding = 0) uniform sampler2D atlas;
layout(set = 0, binding = 1) uniform sampler2D images;

layout(location = 0) in vec2 v_local;
layout(location = 1) in vec2 v_uv;
layout(location = 2) in vec4 v_color;
layout(location = 3) flat in vec3 v_params;
layout(location = 4) flat in vec2 v_half;

layout(location = 0) out vec4 out_color;

// Signed distance to a rounded rectangle of half size `hs`.
float rounded(vec2 p, vec2 hs, float r) {
    r = min(r, min(hs.x, hs.y));
    vec2 q = abs(p) - hs + r;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}

void main() {
    float mode = v_params.z;
    if (mode > 0.5 && mode < 1.5) {
        float coverage = texture(atlas, v_uv).r;
        out_color = vec4(v_color.rgb, v_color.a * coverage);
        return;
    }
    if (mode > 1.5 && mode < 2.5) {
        // Premultiplied texels, filtered, then back to straight alpha for
        // the blend; the quad's alpha fades the image. Rounded corners clip.
        vec4 t = texture(images, v_uv);
        float clip = 1.0;
        if (v_params.x > 0.0) {
            clip = clamp(0.5 - rounded(v_local, v_half, v_params.x), 0.0, 1.0);
        }
        out_color = vec4(t.rgb / max(t.a, 1.0 / 255.0), t.a * v_color.a * clip);
        return;
    }
    if (mode > 2.5) {
        // A soft shadow: the quad is the shadow's rectangle grown by the
        // blur radius on each side.
        float blur = max(v_params.y, 1.0);
        float d = rounded(v_local, v_half - blur, v_params.x);
        float alpha = 1.0 - smoothstep(-blur, blur, d);
        out_color = vec4(v_color.rgb, v_color.a * alpha * alpha);
        return;
    }
    // Rounded rectangle, anti-aliased over one pixel.
    float d = rounded(v_local, v_half, v_params.x);
    float alpha = clamp(0.5 - d, 0.0, 1.0);
    float border = v_params.y;
    if (border > 0.0) {
        alpha *= clamp(d + border + 0.5, 0.0, 1.0);
    }
    out_color = vec4(v_color.rgb, v_color.a * alpha);
}
